# PLAN.md — TAIRiX Build Plan

This plan turns the requirements in `AGENTS.md` into ordered, assignable
work. Each **Stage** is delivered by a separate task (and likely a separate
agent). A stage is complete only when:

- All listed deliverables exist.
- All listed tests pass under `cargo xtask test`.
- All listed documentation is written and links cleanly.
- `AGENTS.md` rules have been observed (no hacks, no duplication, no
  weakened tests, no missing docs).
- The `AGENTS.md` §2.15 validation gate has been run over the **entire**
  workspace and is green: `cargo fmt --all`, the full `cargo xtask ci`
  pipeline, `cargo xtask fuzz --secs 5`, and anything else
  `.github/workflows/ci.yml` exercises (§7 "Definition of done"). No stage,
  and no individual piece of work within it, is complete until this gate
  passes; the actual command output is quoted in the completion report.

Do **not** begin a stage before all its listed dependencies are complete.

---

## Stage 0 — Repository Foundation

**Dependencies:** none.

**Deliverables**
- Workspace `Cargo.toml` listing every planned crate (empty crates allowed
  as placeholders, but each must compile).
- `rust-toolchain.toml` pinning a specific nightly with the required components
  (`rust-src`, `llvm-tools-preview`, `clippy`, `rustfmt`).
- `.cargo/config.toml` declaring per-target build flags and linker scripts.
- `rustfmt.toml`, `clippy.toml`, `deny.toml` (license + advisory rules).
- `tools/xtask/` with subcommands: `build`, `test`, `clippy`, `fmt`,
  `docs-check`, `abi-check`, `c-header`, `font-atlas`, `deps-check`,
  `cfg-check`, `coverage`, `ci`, `image`.
- `docs/` mdBook scaffold.
- CI definition (`.github/workflows/ci.yml` or equivalent) running
  `cargo xtask ci` on every push.
- `tools/ci/`: CI/build-host orchestration — thin wrappers around
  `cargo xtask` for an unattended builder (scheduling, logging, and the
  parallel nightly 24 h soaks). No pipeline logic; that stays in
  `tools/xtask` (§15).
- `LICENSE`, `README.md` (short), `AGENTS.md` (exists), `PLAN.md` (this file).

**Tests**
- `cargo xtask ci` passes on a clean clone.
- Workspace builds for every Tier-1 target with empty crates.

**Docs**
- `docs/src/architecture/overview.md` — one-page system map.
- `docs/src/contributing.md` — points to `AGENTS.md`.

**Status: complete.**
- Toolchain pinned to `nightly-2026-07-03` (rustc 1.98.0-nightly) with
  `rust-src`, `llvm-tools-preview`, `clippy`, `rustfmt` and the four Tier-1
  cross targets. Pin requires cargo-deny ≥ 0.19 (CVSS 4.0 advisories).
- `.cargo/config.toml` declares the `xtask` alias + per-target rustflags;
  `rustfmt.toml`/`clippy.toml`/`deny.toml` enforced via `cargo xtask ci`
  (`cargo deny` passes advisories + bans + licenses + sources).
- `tools/xtask` exposes the closed subcommand set (§7/§14/§17.5): `build`,
  `test`, `clippy`, `fmt`, `docs-check`, `abi-check`, `c-header`,
  `font-atlas` (generates/verifies the Inconsolata glyph atlas in
  `lib/font/src/` from the committed OFL face in `lib/font/assets/mono/`),
  `deps-check`, `cfg-check`, `coverage`, `ci`, `image`.
- CI: `.github/workflows/ci.yml` runs `cargo xtask ci` per push/PR;
  `soak.yml` runs nightly soaks on a self-hosted Linux runner. `tools/ci/`
  holds thin `cargo xtask` wrappers (`ci-run.sh`, `soak.sh`) + scheduler
  samples; no pipeline logic in the scripts (§15).
- `ci` runs each test once (`--once` fuzz/proptest gates); the soak budget
  lives only in the time-limited GitHub soaks. Seed selection/logging/budget
  is the shared `tests/fuzzseed` (`tairix_fuzzseed`) seam: fresh seed per run,
  pinnable via `TAIRIX_{FUZZ,PROPTEST,FSSOAK}_SEED` for replay.
- `LICENSE` is GPL-2.0-or-later with the `TAIRiX-syscall-note` ABI exception.

---

## Stage 1 — Shared Libraries (`lib/`)

**Dependencies:** Stage 0.

**Deliverables**
- `lib/abi`: stable `#[repr(C)]` types for syscalls, IPC messages, manifests,
  errors, capability IDs. Versioned (`abi-v1`).
- `lib/caps`: `Capability`, `CapabilitySet`, delegation/revocation primitives,
  serializable token format (signed by the local authority key).
- `lib/collections`: only collections actually required by stages 2–6.
- `lib/crypto`: thin, audited wrappers around vetted upstream crates
  (e.g. `ring`, `rustcrypto`). No hand-rolled primitives.
- `lib/log`: structured, level-filtered, no-alloc-on-hot-path logging with
  stable event IDs.
- `lib/rng`: random number generation — a NIST SP 800-90A HMAC-SHA256
  CSPRNG composed over `lib/crypto`'s audited HMAC, a pluggable
  entropy / hardware-RNG seam (the §19.2 platform RNG), and a fast
  non-cryptographic xoshiro256++ generator.
- `lib/util`: only items used by ≥ 2 crates.

**Tests**
- Unit tests in each crate, mirror layout (§7).
- Property tests for `lib/caps` delegation rules (a delegated set is always
  a subset of the parent).
- Fuzz target for `lib/abi` decoding.

**Docs**
- One page per crate under `docs/src/`.
- Rustdoc on every public item.

**Status: complete.**
- All `lib/*` crates implemented `no_std` (`abi`, `caps`, `collections`,
  `crypto`, `log`, `rng`, `util`), rustdoc + unit tests per §7; coverage
  clears §7 thresholds.
- `lib/abi` ships the `abi-v1` types + a fuzz harness. `abi-v1` is **not
  frozen yet** (no release); "frozen" elsewhere means the per-type stability
  discipline (a shipped wire layout is not widened in place), not a released
  `abi-v1`.
- `lib/caps` enforces subset-only delegation (exhaustive property test).
- `lib/crypto` exposes audited SHA-256 + Ed25519 verification only; upstream
  crates pinned exactly with `zeroize` enabled.
- `lib/rng` (experimental): `CsRng` HMAC-SHA256 DRBG over `lib/crypto`'s HMAC
  (NIST CAVP-validated) with a pluggable `EntropySource`, a `HardwareRng`
  seam, and `FastRng` (xoshiro256++) — the §19.2 platform-RNG seam.
- `lib/util` holds only items with ≥ 2 independent callers (§2.3):
  `fmt` (audit-field formatters; `kernel/sec` + `kernel/ipc`, promoted in
  Stage 2.5), `size` (the GNU `--block-size` grammar, ceiling block
  scaling, and human-readable renderings; the `du` + `df` command apps,
  `plans/APPS.md` Stage C), `cfloat` (the C-locale printf float renderer;
  the `seq` + `printf` command apps, hoisted out of `seq` when `printf`
  became the second consumer), `cnum` (the C-locale `strtod` scanner
  with longest-prefix `endptr` semantics and exact hex-float rounding;
  the same two apps), `count` (the GNU `-c`/`-n` count-with-multiplier
  grammar; the `head` + `tail` command apps, hoisted out of `head` when
  `tail` became the second consumer), `tailwindow` (the bounded
  rolling "keep the last N bytes/lines" windows; the `head` + `tail`
  command apps — `head`'s `-c -N`/`-n -N` elide modes and `tail`'s
  `-c N`/`-n N` last-N modes are the two policies over one mechanism),
  and `conf` (the `#`-comment line grammar every line-oriented
  configuration store shares; `lib/sysconfig` + `lib/netconfig` +
  `userland/system/init`'s service registry and startup list, hoisted out
  of the four private copies so a comment is recognised identically in
  all of them).
- The cross-checked `syscalls.rs` ↔ `table.rs` pair is reserved for Stage 2
  so `cargo xtask abi-check` always sees both halves.

---

## Stage 2 — Kernel Core (architecture-neutral)

**Dependencies:** Stage 1.

**Deliverables**
- `kernel/core`: kernel entry, panic handler (logs and halts; never silently
  resets), boot-time invariants, global init order.
- `kernel/mem`:
  - Physical frame allocator (buddy + bitmap).
  - Virtual memory manager (per-process page tables).
  - Kernel slab allocator with guard pages.
  - Zero-on-free policy for sensitive regions.
  - `Result`-returning allocation API (no panic on OOM).
- `kernel/sched`: SMP-aware scheduler (per-CPU run queues, work stealing,
  priority + fairness, IPI-based preemption).
- `lib/sync`: spinlocks, RW locks, MCS locks, RCU-equivalent, all
  documented with their use cases.
- `kernel/ipc`: capability-checked message ports, shared memory objects,
  asynchronous notifications.
- `kernel/sec`: user/group/capability tables, manifest verification,
  audit log writer.
- `kernel/syscall`: dispatch table generated from `lib/abi/src/syscalls.rs`.

**Tests**
- Host-side unit tests for every algorithm that does not need hardware.
- QEMU-based integration tests for memory isolation: a test process
  attempting to read another's memory must fault.
- Stress test for the scheduler under load on ≥ 4 emulated cores.

**Docs**
- `docs/src/architecture/kernel.md`, `…/memory.md`, `…/scheduler.md`,
  `…/ipc.md`, `…/security.md`, `…/syscalls.md`.

**Sub-stages**
- [x] 2.1 — `lib/sync`: spinlock + IRQ-safe spinlock, writer-preference
      RwLock, MCS queue lock, SeqLock, `Once`/`OnceCell`; loom + proptest
      tests; decision tree in `docs/src/architecture/sync.md`. The crate needs
      only `core`, never `alloc`, so a freestanding binary that links it is not
      forced to supply a global allocator.
- [x] 2.2 — `kernel/mem`: buddy/bitmap `FrameAllocator` over a typed
      `BootMemoryMap`, per-process `AddressSpace<P: PageTable>` (Arch HAL
      `mmu::AddressSpace + tlb::TlbShootdown` alias, including transactional
      contiguous maps with one range TLB synchronization), guard-page kernel
      `Slab`, `alloc_sensitive`/`free_sensitive` zero-on-free, and
      `Result<_, AllocError>` everywhere (no panic on OOM). Includes the
      early-boot RAM self-test (`kernel/mem/src/ramtest.rs` engine +
      `kernel/core/src/memtest.rs` display): the `Phase::Mem` step tests every
      usable region through the arch direct `PhysMap` before the allocator
      hands out a frame. A quick *boot sanity check*, not an exhaustive march
      test (a few seconds on 8 GiB under QEMU): a whole-window `O(log n)`
      address-line marker walk plus a device stuck-bit test that samples one
      word per 4 KiB (both bit polarities), each read flushed per-word to
      reach DRAM. It does not scrub whole regions — consumers zero their own
      frames. It draws the `TAIRiX <version> <RAM>MiB` identity line as a
      counter (yellow while running, light green when proven) climbing to the
      installed total, coalesced to a bounded number of in-place redraws so
      the animation is smooth on any RAM size, and halts with a red
      failing-MiB location on any fault (fail closed). `init`'s banner no
      longer repeats the version/RAM line — it adds only the processor
      summary beneath it.
- [x] 2.3 — `kernel/sched`: SMP scheduler — per-CPU Chase–Lev work-stealing
      queues, MLFQ priority + fairness with periodic boost, IPI preemption
      hook behind `SchedulerArch`, `spawn`/`park`/`unpark`/`exit`. Host-only
      `TestArch` is `test-arch`-gated.
- [x] 2.4 — `kernel/sec`: `IdentityTable` (users/groups), per-task
      `TaskCapabilities` (effective = user grant ∩ manifest request) with
      delegation/revocation via `lib/caps`, Ed25519 manifest verification
      (fail closed on bad sig/ABI/unknown cap), audit writer; no ambient
      authority (locked by `uid == 0` tests).
- [x] 2.5 — `kernel/ipc`: capability-checked typed message ports (lock-free
      closed-state fast path), capability-gated shared-memory objects over
      `kernel/mem` `SensitiveBuffer` (revocation invalidates all mappings),
      OR-accumulating notifications. Caps checked at bind + every send;
      receivers do not re-check (§5.2). Audit-field formatters promoted to
      `lib/util::fmt` (§2.2).
- [x] 2.6 — `kernel/core`: arch-neutral `kernel_main` (init order
      `log → mem → sec → sched → ipc`), `BootInfo` + `KernelArch` trait for
      arch ports, `handle_panic` logging `KERNEL_PANIC` then halting (never
      silently resets); zero global mutable statics.
- [x] 2.7 — `kernel/syscall`: dispatch table generated from
      `lib/abi/src/syscalls.rs` (`SyscallSpec` table — yield/exit/ipc_send/
      ipc_recv/cap_query/cap_delegate/cap_revoke/clock_get), `ENCODED_TABLE`
      SHA-256-pinned. Kernel `Dispatcher` + `SyscallHandlers` trait,
      type-driven arg validation, `SYSCALL_TABLE_HASH` re-checked at boot.
      `cargo xtask abi-check` recomputes the hash against on-disk + linked
      constant (with a desync negative test); fuzz harness mirrors
      accept/reject. Per-arch entry stubs deferred to Stage 3.
- [x] 2.8 — Stage-2 QEMU integration: `tools/qemu` runner (PVH `-kernel`
      direct boot, `isa-debug-exit`, strict wall-clock budget, no retries) +
      `cargo xtask test --qemu`; `memory_isolation` (two page tables, CR3
      switch, asserts attacker `#PF`) and `scheduler_stress` tests.

**Status: complete.** Sub-stages 2.1–2.8 done; `cargo xtask test --qemu`
green. The scheduler-stress deliverable is satisfied host-side (20 000
tasks / 4 simulated cores) and under QEMU (`scheduler_stress_qemu`: 8 192
tasks / 4 emulated cores under real LAPIC-timer preemption, asserting
`preemption_count(cpu) >= 10` and ≥ 2 dispatching CPUs — delivered with
Stage 3a). The boot QEMU test boots the production `tairix-kernel` pipeline
end-to-end.

---

## Stage 3 — Architecture Ports

**Dependencies:** Stage 2 (interface-level; implementations land in parallel
sub-stages).

**Cross-arch parity burn-down:** bringing `aarch64`, `riscv64`, and
`wasm32` up to (at least) `x86_64` level — finishing the §17.2 Arch HAL
migration, aarch64 SMP/FDT, live-scheduler wiring, and the QEMU vertical
parity sweep — is staged in `plans/WIRING.md` (continuation prompt
`.junie/next-wiring-prompt.md`).

The §17.2 Arch HAL migration and cross-arch parity sweep (WIRING W0–W17)
are **complete** — every enumerated arch primitive lives behind the HAL and
all four ports pass the conformance suite. Summary of what landed:

- [x] W0 — Arch HAL `conformance` harness (`kernel/arch/api`); every port
      has `passes_arch_hal_conformance_suite` over its real handles.
- [x] W1 — Early-boot `PlatformDiscovery` HAL + shared hardware-tree ABI
      (`lib/abi/src/hwtree.rs`, §18.1) + shared `lib/fdt` parser; per-port
      discoverers (x86_64 ACPI/MADT, aarch64+riscv64 FDT, wasm32 host query).
- [x] W2 — `PerCpu` storage HAL (GS-base / `TPIDR_EL1` / `tp` / worker slot).
- [x] W3-A — `IrqController` + `InterruptEntry` HAL (riscv64 PLIC, aarch64
      GICv2 over a `GicMmio` seam, x86_64 IO-APIC controller-only/vectored).
- [x] W3-B — aarch64 device-IRQ QEMU vertical (PL031 RTC SPI via GICv2/EL1).
- [x] W4 — `Timer` HAL slice (callback install/dispatch; arming stays per port).
- [x] W5a — `ContextSwitch` HAL (`TaskContext` save area; wasm32 n/a).
- [x] W5b — MMU/page-table HAL (`AddressSpace` map/translate/unmap, `PageFlags`,
      `TlbShootdown` page/range local-invalidation slice, `PageTableFrames`
      frame source; `kernel/mem` `AddressSpace<P: PageTable>` rides the HAL;
      wasm32 n/a).
- [x] W6 — aarch64 SMP secondary bring-up via PSCI `CPU_ON` + real GICv2 SGI.
- [x] W7 — live `kernel/sched` task switch on aarch64 (timer + IPI drive it).
- [x] W8 — wasm32 multi-worker SMP + live cooperative scheduler (MessageChannel
      IPI, RAF tick).
- [x] W9 — side-channel (§19.1) + memory-tagging (§19.10) completeness verified
      honest on all four ports; `kernel/mem` slab software UAF tag-check is the
      on-by-default floor. KPTI/IBPB/MTE-on-`FEAT_MTE` carried to Stage 6.
- [x] W10 — aarch64 heterogeneous `core_class` (big.LITTLE) from FDT
      `capacity-dmips-mhz`.
- [x] W11 — aarch64 virtio blk/net/display(ramfb)/input QEMU verticals +
      riscv64 virtio-input sibling; shared EL1 identity-MMU bring-up + fw_cfg.
- [x] W13 — cross-CPU TLB-shootdown HAL (`CrossCpuTlbShootdown`): x86_64 IPI,
      aarch64 `tlbi ...is` broadcast, riscv64 SBI RFENCE, wasm32 n/a.
- [x] W14 — `SecondaryBringup` HAL (x86_64 INIT-SIPI-SIPI, aarch64 PSCI,
      riscv64 SBI HSM, wasm32 Web Worker); completes the §17.2 burn-down.
- [x] W15 — all bare-metal/wasm SMP verticals routed through the HAL trait.
- [x] W16 — wasm32 framebuffer display vertical (browser canvas).
- [x] W17 — trimmed aarch64 `virt` DTB embed; device verticals parse the full
      tree at runtime via `tairix_fdt::Fdt`.

Beyond the W-series burn-down, the HAL gained the CPU-feature-detection
slice for the self-optimising routine-selection framework
(`plans/FIX-HARDWARE-FEATURES.md` P1): the closed `CpuFeatures` trait
(deterministic `CpuFeatureSet` + `CoreType` from CPUID /
`ID_AA64ISAR0_EL1`+`ID_AA64PFR0_EL1` / `misa`+`riscv,isa` string / honest
wasm32 host query) and the `CpuCycles` cycle-counter trait (`rdtsc` /
`CNTVCT_EL0` / `time` CSR / `performance.now()`), each with a
`kernel/arch/api` conformance vertical every port passes. The arch-neutral
capability vocabulary (`CpuFeature`/`CpuFeatureSet`) it produces lives in
`tairix_abi::cpufeatures` (the dependency-free ABI crate, mirroring
`tairix_abi::hwtree`) so the HAL and the generic dispatch framework share one
definition without the framework taking a forbidden `kernel/*` edge (§17.4).

The generic dispatch framework itself is `lib/cpuops`
(`plans/FIX-HARDWARE-FEATURES.md` P2, framework landed): a `no_std`+alloc,
platform-neutral crate holding the whole selection abstraction — `Candidate`/
`Family`, the `Selector` (filter on the capability gate → mandatory self-verify
against the portable reference → choose by declared priority or a bounded
median `BenchHarness` over an injected `CycleCounter` → fail closed to the
portable baseline), per-core-type `OpsTables`, operator pins (which still
self-verify), and the typed `Decision`/`DecisionSink` audit seam. Crypto is
availability-only, never benchmarked.

The first P2 consumer has landed: `lib/crc32c` — the one first-party CRC-32C
(Castagnoli) block-integrity checksum, a portable table baseline plus per-arch
hardware candidates (aarch64 `crc32c*`, x86_64 SSE4.2 `crc32`; GPR-only, gated
by a `build.rs`-emitted per-arch cfg the `lib/abi-trap` way so no
`cfg(target_arch)` leaks, host-fuzzed against the reference), selected once and
self-verified through `lib/cpuops`, fail-closed to the baseline. It is delivered
to consumers through the **process's common CPU-feature set**: the kernel folds
each core's detected `CpuFeatureSet` (via the `KernelArch::cpu_features` HAL
handle) into the migration-safe intersection (`kernel/core::cpuops`), stamps it
into every process's startup vector (`ProcessStart::cpu_features`, exposed by
`lib/rt::cpu_features`), and resolves the in-kernel families against it after
SMP bring-up (auditing each choice, `AuditEvent::CpuOpsRoutineSelected`). ARXFS
routes its fast `physical_checksum` through `lib/crc32c` (replacing the former
FNV-1a; on-disk physical-integrity trailer 8→4 bytes, pre-release format change,
`docs/src/filesystem/arxfs-spec.md`). The `lib/crypto` SHA-256 backend-
availability seam (capability-gated, never benchmarked, with a boot-time FIPS
known-answer self-test) has also landed.

The P3 **page-zero** family (`lib/pagezero`) has landed as a second
capability-gated (`ByPriority`) consumer: a portable byte-fill baseline plus
per-arch hardware candidates (aarch64 `DC ZVA`, x86_64 ERMS `rep stosb`; new
`DcZva`/`Erms` `CpuFeatureSet` bits, `build.rs`-emitted per-arch cfg,
host-fuzzed), selected once and self-verified bit-identical through
`lib/cpuops`, fail-closed. The `kernel/mem` frame scrub (`zero_frame` /
`fill_frame`) routes through it. This corrected a plan error — page-zero (and
`memcpy`/`memset` where a hardware fill dominates) is a *capability* decision,
not `ByBenchmark`. Remaining P3/P4 work: the genuine `ByBenchmark` consumers
(userland `lib/raster` blit, `lib/net` RFC-1071 checksum) are **blocked** on an
undesigned userland-measurement path (the cycle counter is kernel-only); the
former XOR/parity family is dropped until a real RAID/FEC consumer exists; plus
the aarch64 hardware-crypto backend, the per-arch QEMU verticals, and the P0
build-time floor raise if a minimum requirement is documented.

Each sub-stage delivers one architecture. They share the same checklist:

- Boot stub (minimal assembly, justified per `AGENTS.md` §1).
- Early console (serial/UART/framebuffer/WASM console).
- MMU / page-table primitives wired into `kernel/mem`.
- Context switch + interrupt entry/exit.
- Timer + IPI plumbing for `kernel/sched`.
- Per-arch syscall entry.
- QEMU run script in `tools/qemu/<arch>.rs`.

**Sub-stages**
- [x] 3a — `kernel/arch/x86_64` (BIOS + UEFI boot, APIC, ACPI minimal):
      multiboot2/UEFI memory-map hand-off → `BootMemoryMap`, ACPI MADT parse,
      LAPIC/IO-APIC + LAPIC-timer calibration, AP startup via INIT-SIPI-SIPI,
      interrupt prologue + live-scheduler preemption, per-arch syscall entry,
      and the production `kernel/tairix-kernel` bin booting `kernel_main` to
      `BootCompleted` under QEMU.
- [x] 3b — `kernel/arch/aarch64` (QEMU `virt`; GICv2, generic timer, PSCI SMP).
      Full Arch HAL impl; PL011 console, stage-1 MMU, context switch, EL1
      vectors. Real Raspberry Pi 4 (BCM2711) bring-up is staged in `plans/PI.md`.
      Console output defaults to the attached display (the `video` framebuffer
      boot console over the shared `lib/vcmailbox` VideoCore mailbox protocol
      crate, `plans/PI.md` P7b) with the UART as the fallback.
- [x] 3c — `kernel/arch/riscv64` (QEMU `virt`; PLIC, CLINT, SBI).
- [x] 3d — `kernel/arch/wasm32` (browser sandbox; cooperative scheduling over
      `requestAnimationFrame` / `MessageChannel`; isolation via WASM linear
      memory between worker contexts).

**Tests (per sub-stage)**
- Boots to `init` placeholder in QEMU / browser headless harness.
- Memory-isolation test passes.
- Timer interrupt drives scheduler.

**Docs**
- `docs/src/platform/<arch>.md` with build, run, and debug instructions.

**Status: complete.** All four Tier-1 ports are complete and pass the Arch HAL
conformance suite, so Stage 3 is complete. The production syscall wiring the
x86_64 bin's fail-closed dispatch callback awaits is tracked in the Stage 2.7
follow-up section below.
---

## Stage 2.7 follow-up — Production syscall wiring

**Dependencies:** Stage 2.7 (`kernel/syscall::Dispatcher`), Stage 3a
(`kernel/tairix-kernel` bin with fail-closed dispatch callback).

**Status: complete.** Wired the production syscall path end-to-end (sub-items
f1–f7):
- f1 — per-CPU current-task slot in `kernel/sched` (`current_task`,
  `yield_current`).
- f2 — `TaskId → &TaskCapabilities` CapTable registry in `kernel/sec`.
- f3 — production `SyscallHandlers` impl in `kernel/core` + `monotonic_ns`.
- f4 — `DispatchCallbackSlot` + `Phase::Syscall` registration hook in
  `kernel/core`.
- f5 — `production_dispatch` swap + `DISPATCH_SLOT` install in
  `kernel/tairix-kernel`.
- f6 — `tairix-test-syscall-dispatch-qemu` QEMU test driving
  `(cap_query, CAP_TIME_SET)` + `(exit, 0)`, verified via audit sink.

`ipc_send`/`ipc_recv` and `cap_delegate`'s `set_ptr` copy-in are explicitly
deferred to later stages (not stubbed, §15.1).
---

## Stage 4 — Driver Framework and First Drivers

**Dependencies:** Stage 2 + at least one Stage 3 sub-stage.

**Deliverables**
- [x] `lib/abi/src/driver/` driver traits per class
  (`Display`, `Filesystem`, `Block`, `Net`, `Input`, `Bus`).
- Driver host in `userland/` that loads/unloads `.rxe` driver modules,
  enforcing capabilities at load time.
- Initial drivers:
  - `drivers/display/vesa` (x86_64 BIOS).
  - `drivers/display/framebuffer` (aarch64 Pi, riscv64 virt, wasm32 canvas).
  - `drivers/bus/pci` (x86_64), `drivers/bus/mmio` (aarch64/riscv64),
    `drivers/bus/virtio` (cross-arch).
  - `drivers/storage/virtio_blk`.
  - `drivers/input/ps2` (x86_64), `drivers/input/usb_kbd` +
    `drivers/input/usb_mouse` (HID boot-protocol class drivers over the
    URB transport, `plans/USB.md`).
  - `drivers/network/virtio_net`.

**Tests**
- Mock-host unit tests for each driver.
- QEMU integration: load driver → use device → unload driver → reload.

**Docs**
- `docs/src/drivers/overview.md` and one page per driver class.
- Each driver crate ships a `README.md` (supported HW, caps, limits).

**Status: in progress.**
- Driver trait surface (`lib/abi/src/driver/`: `Display`, `Filesystem`,
  `Block`, `Net`, `Input`, `Bus`, `DriverHost`) and the driver host
  (`userland/system/drvhost`) shipped; the host enforces `CAP_DRV_LOAD` at the
  §8 load gate.
- First drivers shipped: `display/vesa` (x86_64 VBE), `display/framebuffer`,
  `input/ps2` (x86_64), `bus/pci` (x86_64 PIO mechanism #1 + cross-arch PCIe
  ECAM, plus the BCM2711 *windowed* index/data config mechanism
  `mechanism_brcm` for the Pi 4 VL805 path), `bus/pcie_brcm` (the BCM2711
  PCIe root-complex link bring-up, host-tested and metal-confirmed via the
  user-space USB-HID chain, `plans/PI.md` P10),
  `bus/mmio`, `bus/virtio`
  (+ in-kernel `KernelVirtioHost` with the owned-`DmaSlab` DMA shape),
  `storage/virtio_blk`, `network/virtio_net`. Each emulable driver has a
  `load → use → unload → reload` QEMU vertical; the shared `fw_cfg`/ramfb DMA
  protocol lives once in `lib/fwcfg` (`tairix-fwcfg`, §2.2 — also the aarch64
  framebuffer boot console's QEMU `virt` ramfb backing). The Pi 4 EMMC2
  SD-host driver (`drivers/storage/emmc2`, an Arasan/SDHCI-5.1 SD block
  driver: ADMA2 DMA with a PIO fallback, **interrupt-driven completion** — the
  command/transfer waits park on the controller's bound GIC line through a
  `CompletionWait` seam rather than busy-spinning, §17.1/§2.16) ships its
  read and write paths host-tested against a register-level mock; it has no
  QEMU vertical (QEMU models no Pi EMMC2); its PIO path is accepted on metal
  (reads the FAT boot partition + ARXFS root off a real Pi 4 SD card), while
  the cache-synchronized DMA path remains metal-gated (`plans/PI.md` P8).
- DMA goes through `kernel/sec::dma` (`CAP_MMIO_MAP`/`MEM_DMA` checked, audited);
  MMIO is reached only through the capability-gated `KernelMmioMapper`.

**Remaining (tracked in `.junie/next-session-prompt.md`):** interrupt-driven
ps2/virtio wake-ups (polled cooperative shim today), packed virtqueues
(virtio 1.1 §2.7 — Stage 5 follow-up), the riscv64 virtio QEMU verticals not
runnable in this environment, and the Stage 4.D acceptance gate.

**Deferred mechanism (delivered by Stage 4.HW):** drvhost hands every
verified, signature-checked image to its `DriverSpawner` seam
(`userland/system/drvhost/src/spawner.rs`), which completes the driver's
registration in its own protection domain and reports the outcome — the
host never holds an entry pointer into the image (the former in-image
`EntryResolver` is deleted). The kernel-side spawn path behind the seam
is proven (Stage 4.HW increment 1): a verified `/System/Drivers/`
payload is spawned through the parameterised production producer
(`Aarch64ProcessSpawn::spawn_with` + the exported `KernelSpawnCtx`) and
completes the `DriverRegisterReply` handshake over the production
`ipc_send` path. drvhost deployments and QEMU verticals still register
in-process through the seam until the `DriverHost` surface (DMA, MMIO)
is reachable over IPC.

---

## Stage 4.HW — Hardware Detection and Driver Autoload

**Dependencies:** Stage 4 (driver host + bus drivers) and the Stage 3
sub-stages for each target's early-boot platform discovery.

This stage implements `AGENTS.md` §18: detect the hardware present at
boot and autoload the matching drivers, with no hand-maintained static
device list.

**Deliverables**
- `lib/abi/src/hwtree.rs`: the architecture-neutral **hardware tree** ABI
  type (§18.1). Versioned, hashed, frozen on release like the syscall
  table (§9) and sysinfo (§16.6); each node carries a stable id, parent,
  device class, match keys (DT `compatible`, PCI `vendor:device:class`,
  USB `vid:pid:class`, virtio id, MMIO `compatible`), and its resource
  requirements expressed as capability-grant requests (never ambient
  handles).
- Per-architecture discovery that emits the hardware tree, living **only**
  under `kernel/arch/<target>/` as part of the Arch HAL "early-boot
  platform discovery" (§17.2):
  - `aarch64`, `riscv64`: FDT/DTB → hardware tree.
  - `x86_64`: ACPI (+ UEFI/firmware hand-off) and legacy fallbacks →
    hardware tree.
  - `wasm32`: host-environment capability query → hardware tree.
  - Bus children enumerated by `drivers/bus/*` are attached as nodes.
- `userland/system/devmgr`: user-space device manager that reads the
  hardware tree, matches nodes against each driver manifest's **bind
  table**, and autoloads matching drivers through the §8 driver-host
  load gate under `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`. Deterministic match
  resolution; fail-closed; every match/load/skip/failure logged through
  `lib/log` with a stable event ID. The candidate set is **scanned from
  the installed signed bundles** under `/System/Drivers/` at runtime, never
  a compiled-in driver list (§18.6); only the bootstrap floor that must
  exist before the store is reachable is compiled in, matched in-kernel
  through the same shared `lib/devmatch` policy.
- Driver-manifest **bind table** (§8, §9): drivers declare the match keys
  they bind to. Wire it into the existing signed-manifest path.
- Runtime path: hotplug/removal updates the tree and triggers
  load/unload (§8). Unbound nodes are logged, never an error (§18.4).
- A privileged System Information API query (`CAP_SYSINFO_HW`, §16.6)
  that exposes the hardware tree read-only to tools; no `/proc`/`/sys`.

**Tests**
- Host unit tests for `lib/abi::hwtree` encode/decode and ABI hashing.
- Host unit tests for `devmgr` matching: exact match, multi-match
  priority resolution, unbroken-tie rejection, no-match → unbound,
  capability-denied load fails closed.
- Per-arch host tests that the discovery code normalises a sample
  FDT / ACPI / host descriptor into the expected tree.
- QEMU integration per Tier-1 target: boot → devmgr autoloads the
  input/display/storage/network drivers for the emulated devices →
  device usable; headless image leaves the display node unbound and
  reaches text login without error (§17.3).

**Docs**
- `docs/src/drivers/hardware-detection.md` mirroring `AGENTS.md` §18.
- Update `docs/src/drivers/overview.md` and `docs/src/abi/` for the
  hardware-tree type and the `devmgr` service.

**Status: in progress — reprioritised as the next stage of work**, ahead of
the remaining `plans/PI.md` Arc-C metal items (P8 binds the EMMC2 driver
through `devmgr`, so it depends on this stage; current direction lives in
`.junie/next-pi-prompt.md`). The prerequisites the drvhost resolver
deferral was waiting on have landed: the `rxe` loader
(`lib/abi/src/rxe.rs`), `kernel/mem::map_image`/`build_process_image`, the
`EnterUser` HAL primitive on all three native ports, live syscall/IPC
dispatch, and `lib/abi/src/hwtree.rs` with aarch64 FDT discovery. Delivery
order (one fully-gated increment each):
1. **drvhost process spawn — done.** The host side: the in-image
   `EntryResolver` is deleted and `Host` hands the verified manifest +
   payload to the `DriverSpawner` seam
   (`userland/system/drvhost/src/spawner.rs`), which completes the
   registration and returns the outcome — no entry pointer crosses back
   into the host; the verification half (signature, ABI version + syscall
   hashes, `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) is unchanged. The IPC
   register-handshake wire surface: the versioned `DriverRegisterReply`
   record (`lib/abi/src/driver/register.rs`, fail-closed decode, mirrored
   into `include/` as `tairix_driver_register_reply_t`), the `lib/rt`
   `ipc_send` wrapper, and the `lib/rt` startup-argument accessors
   (`tairix_rt::arg` / `arg_count`, published by `_start` from the
   validated startup vector) the child uses to receive the reply endpoint
   id. The kernel-side production spawner: the aarch64 producer is
   parameterised as `Aarch64ProcessSpawn::spawn_with(rxe, ctx, caps,
   args)` (the `spawn` syscall path delegates with the fixed session
   grant; `USER_IMAGE_BIAS` pins the image-bias contract for externally
   converted payloads), and `kernel/core` exports `KernelSpawnCtx` — the
   same admit context the `spawn` syscall handler uses — so a kernel-side
   driver spawn drives the identical production admit path. Proven by the
   `tests/integration/driver_spawn_qemu_aarch64` `-M virt` vertical: a
   verified `/System/Drivers/` payload spawned with driver-class caps +
   the reply endpoint id in `arg(1)` (stub:
   `tests/integration/driver_register_program`), the production
   `KernelDispatchHook` servicing the stub's syscalls (copy-in +
   capability-gated `Port::send`), and a budget-bounded cooperative
   `Port::recv` wait host-side decoding the reply fail-closed. In-process
   `DriverSpawner` impls remain only in tests/verticals until the
   `DriverHost` surface (DMA, MMIO) is reachable over IPC.
2. **Bind table — done.** `DriverManifest` carries `bind_key_count` (in
   place of the reserved byte, §2.13) and the `.rxe` body is now
   capability list + bind table + payload, all signature-covered. One
   entry is a `DriverBindKey` (`lib/abi/src/driver/mod.rs`): an
   `HwMatchKey` plus the §18.3 deterministic bind `priority`
   (higher-priority match binds; an unbroken tie is a packaging
   defect), decoded fail-closed through the single shared
   `decode_bind_keys` and capped at `DRIVER_MANIFEST_MAX_BIND_KEYS`
   (16, a validation bound §24.4). drvhost validates every entry at the
   load gate (`HostError::BindKeyInvalid`, audit `EventId(7011)`)
   before the spawner hand-off; the C view (`tairix_driver_bind_key_t`,
   `TAIRIX_DRIVER_MANIFEST_MAX_BIND_KEYS`, `TAIRIX_DRIVER_BIND_KEY_WIRE_LEN`)
   is regenerated. Docs: `docs/src/abi/driver_traits.md`,
   `docs/src/drivers/{host,lifecycle}.md`.
3. **`userland/system/devmgr` — done.** The matcher/autoloader crate
   (`tairix-devmgr`, `no_std`, `lib/*` deps only per §17.4):
   `DeviceManager::autoload` walks the hardware tree, resolves each
   non-root node against every `DriverCandidate`'s decoded bind table
   (`matcher::resolve` — strictly highest matched priority wins; an
   unbroken cross-candidate tie is refused as a packaging defect,
   §18.3), leaves unmatched nodes unbound-and-logged (§18.4, never an
   error), loads each winner exactly once through the injected
   `DriverLoader` seam (implemented by the deployment over the drvhost
   `Host::load` gate, mapping `HostError::as_errno`), and fails a
   refused load closed for that node only. Audit: stable `EventId`
   range `13000..14000` (`NODE_BOUND`/`NODE_UNBOUND`/
   `NODE_TIE_REJECTED`/`NODE_LOAD_FAILED`). Proven by 16 in-crate unit
   tests plus the end-to-end `tairix-drvhost --test devmgr_autoload`
   composition test (signed `.rxe` bind tables decoded by the real
   gate, real `Host` loads, missing-`CAP_DRV_LOAD` refusal). Docs:
   `docs/src/drivers/hardware-detection.md`. Remaining for the stage:
   the hotplug/removal runtime path (the `CAP_SYSINFO_HW` read-only
   exposure already ships via `sysinfod`'s `HARDWARE_TREE` query,
   Stage 6).
4. **Generic match-key emission — done.** The aarch64
   `FdtDiscovery::discover` (`kernel/arch/aarch64/src/platform.rs`) is
   a generic device-tree walk, confined to the port (§18.2): every
   `compatible`-carrying node is emitted with its compatible strings
   as match keys (devicetree most-specific-first order, capped by the
   `HW_NODE_MAX_MATCH_KEYS` ABI bound), `/memory` nodes classified by
   `device_type`, `reg` decoded with the parent's
   `#address-cells`/`#size-cells` and translated through ancestor-bus
   `ranges` into CPU-physical MMIO resources (untranslatable entries
   dropped, never emitted untranslated), `interrupts` (three-cell GIC
   specifiers) emitted as IRQ resources, classes derived from
   `device_type`/`interrupt-controller`/name stem
   (`tairix_fdt::name_stem`), interior buses emitted as `Bus` parents,
   and unbindable nodes (no representable key, not memory) omitted.
   Per-device augmentations only the platform's tree can size are
   kept: the VideoCore mailbox property-buffer carve (P7, a `Dma`
   request) and, on the BCM2711 PCIe host bridge (`brcm,bcm2711-pcie`,
   P10, the VL805 USB host path), *both* of its address windows — the
   inbound-DMA aperture from the node's `dma-ranges`
   (`fdt::dma_ranges_aperture`, emitted as
   `HwResource::dma_translated(top, len, inbound_pcie_base)` — the
   CPU-reachability top/extent plus the inbound PCIe-space base the
   inbound BAR is programmed at) and the outbound MMIO window from its
   `ranges` (`fdt::outbound_mmio_window`, emitted as
   `HwResource::bus_window(cpu_base, size, pcie_base)` — the CPU↔PCIe
   translation the bridge forwards). The orphaned
   per-device finders
   (`fdt::find_mailbox`/`DiscoveredMailbox`/`timer_ppi`) are deleted
   (§2.14); `lib/fdt` exports the shared `read_cells`/`name_stem`
   helpers (§2.2). Proven by the port's platform unit tests (virt/Pi
   fixture shapes, nested-`ranges` translation, fail-closed
   missing-`ranges`/overlong-compatible/depth-bound cases) and the
   Arch HAL discovery conformance vertical. Docs:
   `docs/src/platform/aarch64.md`,
   `docs/src/drivers/hardware-detection.md`.
5. **Migrate the Pi USB-keyboard bring-up chain onto autoload; delete the
   in-kernel composition module — in progress.** The
   `kernel/tairix-kernel::usb_keyboard` module is a P10 in-kernel bring-up
   *scaffold*: the one place §17.4 lets the image binary name the four
   driver crates of the Pi 4 chain (`pcie_brcm` → `pci::mechanism_brcm`
   → `bus_usb` → `input_usb_hid`). It does not scale — answering "more
   boards" by hand-writing one composition module per board is the
   §2.2/§2.3 sprawl this stage exists to prevent. The steady state is the
   data-driven §18 path: every chain node is discovered into the `hwtree`
   and `devmgr` autoloads the matching driver against its signed bind
   table, so adding a board becomes match **data**, not a new composition
   module.

   **Two-tier driver-set invariant (`AGENTS.md` §18.6).** The *set* of
   drivers the system can load is discovered at runtime by scanning the
   installed signed bundles under `/System/Drivers/`, never frozen in a
   kernel array — no build can enumerate every future bus/vendor/interface,
   so new hardware support is a dropped-in signed bundle, not a recompile.
   The only compiled-in exception is the irreducible **bootstrap floor**
   (root-complex/bus bring-up + the storage path) that must exist before the
   store is reachable. The `kernel/tairix-kernel::driver_catalog`
   (`IN_KERNEL_DRIVERS`) is the in-kernel candidate list. Its legitimate
   floor is the **storage path** — the block drivers that read the volume
   holding the store: `tairix_drv_storage_virtio_blk` (virtio device id 2,
   the QEMU `virt` / x86_64 root) and `tairix_drv_storage_emmc2`
   (`brcm,bcm2711-emmc2`, the Raspberry Pi 4 SD card), each registered with
   the driver crate's own `pub const BIND_KEYS` and a build-signed manifest
   (done) — plus the **bus chain** that reaches it. It is still **over-
   broad** in one place: `usb_hid` is a plain HID leaf driver, not
   bootstrap-floor, and must move out to the discovered tier (chunks 5d–5e).
   Both tiers bind by discovery-match through the one shared `lib/devmatch`
   policy and are signed + capability-gated alike; the floor only shrinks
   toward the store (§18.5/§18.6), it never grows.

   The scaffold is now **live**: the aarch64 boot path invokes it
   as an in-kernel keyboard *service kthread* (`plans/PI.md` P10 — the new
   `kernel/tairix-kernel::keyboard_service`, the `kernel/core`
   `InitSpawnCtx::spawn_kernel_service`/`static_frames` seam, and
   `platform::pcie_bringup`), so a metal Pi 4 can drive the video-console
   login from a USB keyboard today; the autoload migration below is the
   steady state that retires the hand-written composition. Staged
   sub-increments (one fully-gated landing each):
   - **5a — driver-declared bind tables + class-wildcard matching — done.**
     Each chain driver crate owns its canonical bind table as a
     `pub const BIND_KEYS` (`tairix_drv_bus_pcie_brcm` → compatible
     `brcm,bcm2711-pcie`; `tairix_drv_bus_usb` → xHCI PCI class
     `0x0C0330`, vendor/device wildcard; `tairix_drv_input_usb_hid` → HID
     boot keyboard `0x030101` + mouse `0x030102`, vendor/product wildcard)
     — the single source of truth a signed manifest's bind table is
     authored from (§18.3). `HwMatchKey`'s constructors are now `const`
     (so a bind table is a `const`), and `HwMatchKey::matches` adds the
     PCI/USB class-with-optional-vendor/device wildcard the matcher
     (`tairix_devmgr`) resolves against, so a generic class driver binds
     without hard-coding a device id while a vendor-specific driver still
     outranks it by priority. abi-v1 unfrozen: no `#[repr(C)]` layout
     changed, so no C-header drift. Docs:
     `docs/src/drivers/hardware-detection.md`.
   - **5b — runtime hardware-tree child attachment.** Let the bus drivers
     emit their enumerated children as `HwNode`s parented into a growable
     tree, so the downstream chain nodes exist for matching (§18.2).
     Host-provable.
     - **5b-i — PCI child emission — done.** `PciBus::describe_function(bdf,
       parent_id, node_id)` (lib/abi) returns the enumerated function as a
       child `HwNode` carrying one `HwMatchKey::pci` of its `vendor:device`
       and **full 24-bit class** (`base<<16|sub<<8|prog_if`, read from config
       dword 2 — the 16-bit `BusDevice::class` drops prog_if, so an xHCI host
       `0x0C_03_30` is told apart from older USB host classes), with the
       `HwDeviceClass` derived from the base class; absent function (all-ones
       vendor) fails closed `NotFound` (§2.9/§18.5). `Pci<C>` implements it
       (inherent `describe_function`/`read_class_24`); the tree owner assigns
       ids and resources are minted at the load gate (§4/§5.4). New trait
       method only — no `#[repr(C)]`/C-header drift. The generic xHCI driver's
       wildcard `BIND_KEYS` (5a) resolves against the emitted VL805 node.
     - **5b-ii — USB/HID child emission — done.** `bus_usb` emits the
       enumerated HID device under the VL805 as a child `HwNode` keyed by its
       **interface** class (`0x03_01_01` keyboard / `0x03_01_02` mouse).
       The enumeration (`UsbDevice::bring_up` /
       `UsbDevice::attach_root_port`) reads the whole configuration
       descriptor and parses every default-alternate interface descriptor
       (`InterfaceInfo::decode_all`, fail-closed bounded walk by each
       `bLength`; a composite keyboard+mouse receiver gets one entry and one
       node per served interface): the discovered `bConfigurationValue` /
       `bInterfaceNumber` drive `SET_CONFIGURATION` / per-interface
       `SET_PROTOCOL(boot)` (no longer hard-coded `1` / `0`), and each
       captured 24-bit interface class is held as that entry's identity. The new
       `UsbDevice::describe_device(parent_id, node_id)` returns an `HwNode`
       (class `Input`) carrying one `HwMatchKey::usb` of the device's
       `vid:pid` + that captured interface class — never fabricated
       (§18.5) — fail-closed `NotFound` before enumeration; the
       `usb_hid::BIND_KEYS` class-wildcard keys resolve against it. A new
       method only — no `#[repr(C)]`/C-header drift. Host-proven (the
       `InterfaceInfo::decode_all` fail-closed cases, the emitted-node match,
       the pre-enumeration refusal).
     The remaining sub-increments turn the bring-up *around* — from a
     hand-composed module that hunts for the keyboard to data-driven
     discovery + `devmgr` autoload — one fully-gated landing each. A live
     VL805 path is a §0.4 metal-acceptance item (no Pi-board QEMU vertical),
     so each chunk touching it lands host tests **plus** a metal checklist;
     the operator supplies the UART/debug log before the next chunk starts.
     Security and correctness are the floor: every load capability-gated
     **and** signature-verified, every input validated, fail closed (§5.4 /
     §23.1).
     - **5c-i — match-driven bring-up decision — done.** The match policy
       (`resolve` / `best_bind_priority` / `DriverCandidate` /
       `MatchResolution`) was lifted out of `devmgr` into the shared
       **`lib/devmatch`** crate, so the kernel reaches the *one* §18.3
       definition without a kernel→userland edge (§2.2 / §17.4; `devmgr`
       re-exports it unchanged). The production **driver-candidate
       catalogue** (`kernel/tairix-kernel::driver_catalog`) pairs each chain
       driver's canonical `BIND_KEYS` (`pcie_brcm` / `bus_usb` / `usb_hid`)
       with its `/System/Drivers/` image path — authored from the crates'
       tables, never re-typed (§2.2) — and `resolve_chain` resolves a
       discovered node against it. `keyboard_service` no longer brings the
       bus up because a bridge address exists; `resolve_discovered_bridge`
       resolves the discovered `brcm,bcm2711-pcie` identity
       (`platform::PCIE_COMPATIBLE`, the discovery contract — never a
       fabricated key, §18.5) against the catalogue and proceeds **only on a
       bound `Winner`** (audit `EventId(4112)`), leaving an unmatched/tied
       node unbound + logged and the service unstarted (§18.4 / §2.9).
       Host-tested (catalogue ↔ emitted VL805/HID/pcie nodes, unmatched and
       fail-closed paths); the freestanding aarch64 kernel builds with the
       gate wired. **Metal checkpoint (operator, §0.9):** re-flash, confirm
       the on-screen `Username:` prompt still takes keystrokes (parity), and
       supply the UART debug log showing the `4112` bound record.
     - **5c-ii — load through the drvhost `Host::load` gate — done.** The
       in-kernel chain bring-up is admitted through the signed-manifest
       `drvhost::Host::load` gate, not a bare `register()` call. `build.rs`
       bakes an Ed25519-signed `DriverManifest` per chain driver (kind
       `InKernel`, the kernel's `SYSCALL_TABLE_HASH`, `CAP_DRV_LOAD`, the
       crate's own `BIND_KEYS`) and embeds the build's driver-signing public
       key as the kernel's trust anchor; `driver_loader::ChainDriverLoader`
       runs the full load pipeline (signature + `CAP_DRV_LOAD` /
       `CAP_DRV_KERNEL` gates, bind-table validation, in-process `register()`
       hand-off). `keyboard_service::bring_up_keyboard_into_tree` admits
       `pcie_brcm` + `bus_usb` before bring-up and re-matches the enumerated HID child
       (`bring_up_keyboard` now yields the keyboard + its
       `UsbDevice::describe_device` `HwNode`) to admit `usb_hid` before the
       report pump — fail closed at each step (`AGENTS.md` §5.4). The chain
       `register()`s are admission-only, so the gate uses a plain `Host`; the
       real MMIO/DMA runs over the service's own capability-gated `ChainHost`.
       `tairix-drvhost` is now an aarch64 dependency; audited at
       `EventId(4132)`. Host-tested (`driver_loader`); aarch64 kernel builds.
     - **5d-0 — the `DriverHost` DMA/MMIO surface reachable over IPC** (the
       standing gap, increment 1's remainder): a user-space driver maps its
       register windows and carves its DMA region over capability-gated
       IPC, with every check staying kernel-side (§5.4). A multi-step
       landing; host-provable plus a `-M virt` vertical where a virtio
       device stands in for the metal controller. **Security foundation
       landed:** the `mmio_map` `abi-v1` syscall (no. 26, `CAP_MMIO_MAP`,
       audited) maps a *granted* device window into the calling driver's own
       address space — the driver names an unforgeable, kernel-issued
       device-resource **grant handle** (never a raw phys, §4), the
       kernel-core handler resolves it owner-checked against the calling task
       through the per-task grant table (§5.4 forgery defence), validates it
       names a memory window (`devres::mappable_window`), and maps only that
       region through the architecture `devres::MmioMapFacility` producer
       (§18.3). The map facility defaults to a fail-closed NULL producer
       (`NotImplemented`), mirroring how `mem_map` shipped its handler before
       SP5b. **5d-0-ii (a) — the concrete grant table — landed:** the
       per-task device-resource grants live in
       `kernel/core::aspace::AddressSpaceRegistry` alongside the task's
       streams and limits (`mint_grant` issues a per-task, monotonic,
       never-reused handle from `1`; `grant` resolves it owner-checked;
       `withdraw` reclaims every grant when the task exits — co-located so a
       parallel per-task registry is avoided, §2.2). The placeholder
       `devres::ResourceGrants` trait / `NULL_RESOURCE_GRANTS` seam from the
       security foundation was deleted in place (§2.13 / §2.14). Host-tested
       (the grant store + the 5 `mmio_map` handler tests minting real grants);
       no ABI/C-header change. **5d-0-ii (b) — the guarded borrowed-space
       MMIO mapper mechanism — landed:** `kernel/mem::mmio::MmioWindowMap` is
       the per-task guarded MMIO virtual-window allocator (bounded window +
       slot bitmap + per-region guard/data accounting) that maps a device
       window into a **borrowed** `&mut AddressSpace<P>` — `NO_CACHE`, never
       executable (W^X §19.2), unmapped guard pages bracketing every window
       (§4), all-or-nothing fail-closed unwind (§2.9) — the device-window
       analogue of `kernel/mem::anon::map_anonymous` and the mechanism the
       production `MmioMapFacility` drives. The owned `MmioMap`
       (`KernelMmioMapper`'s in-kernel mapper) is now a thin wrapper
       delegating to it (§2.2, no consumer churn); host-tested (8
       borrowed-space + 15 existing `MmioMap` tests); no ABI/C-header change.
       **5d-0-ii (b′) — live-address-space retention + production producers
       — landed (aarch64).** `kernel/mem::live` (`LiveUserSpace` object-safe
       `Send` trait + generic `LiveSpace<P, M>`) retains a task's live
       *mutable* `AddressSpace<P>`; `kernel/core::kthread` owns it per task
       and publishes a pointer on a per-CPU `USER_LIVE_SPACE` slot (cleared on
       switch-back, exactly as `USER_RESUME`), reached by
       `with_current_live_space` from the task's own syscall path — never a
       shared lock over a live page table. The `kernel/core::live_producer`
       `LiveMemMap`/`LiveMmioMap` are the `MemMap`/`MmioMapFacility`
       producers (arch-generic, `&'static A`, fail closed when no space is
       retained). The retention is wired into production: the
       `admit_init`/`admit_process` seam carries an
       `Option<Box<dyn LiveUserSpace + Send>>` (x86_64/riscv64 pass `None`),
       the aarch64 `init_spawn`/`spawn_producer` freeze a snapshot **and**
       retain a `LiveSpace`, admitting via
       `spawn_user_kthread_with_stack_live`, and `kernel_main` installs the
       producers for every port. The Arch-HAL `PageTableFrames` gained a
       `Sync` supertrait (every impl already `Sync`) so a port's
       `AddressSpace` is `Send`; aarch64 grew `el0_device_leaf_attrs`
       (`AP_RW_EL0`) so a user-space driver's `mmio_map` window is
       EL0-readable. Proven by the `mmio_map_qemu_aarch64` `-M virt` vertical
       (a spawned EL0 program maps a minted virtio-mmio grant via `mmio_map`
       and reads the device `MagicValue`); new `tairix_rt::mmio_map`; no
       ABI/C-header change. **5d-0-ii (c) — non-`FIXED` `mem_map` placement
       allocator — landed.** `kernel/mem::AnonWindowMap` (bump cursor +
       free-list, §24.1-scalable — a large VA window costs no RAM until the
       frame allocator backs a mapping, which fails closed as deterministic
       OOM) chooses the base for a non-`FIXED` anonymous mapping out of a
       per-task heap window; `LiveSpace` composes it with the audited
       `map_anonymous`/`unmap_anonymous` (one mapping path, §2.2), exposing
       `map_anonymous_placed` and releasing the placement record on unmap
       (fail-closed validate before any teardown). `LiveMemMap::map` routes
       non-`FIXED` requests there (FIXED still uses `addr_hint`); every port's
       `init_spawn`/`spawn_producer` place the window at
       `spawn_layout::ANON_WINDOW_OFFSET` (4 GiB above the image bias — the
       topmost user region, above the device/DMA/shared windows) and size it
       from discovered RAM via `user_windows::user_windows` (physical RAM
       clamped to half the addressable user VA above the base, floored at
       16 MiB; the demand-paged `file_map` window takes the remainder —
       `docs/src/architecture/memory.md` §7f/§7o),
       never a fixed `const` ceiling (§24.1). Proven by `kernel/mem` +
       `kernel/core`
       host tests and the extended `mmio_map_qemu_aarch64` `-M virt` vertical
       (the EL0 program now also round-trips a placed `mem_map`: map → write →
       read-back → `mem_unmap`); no ABI/C-header change. **5d-0-ii (c) DMA
       half — landed.** New `abi-v1` syscall **`dma_alloc`** (no. 27,
       `CAP_MEM_DMA`, audited): it resolves an owner-checked **`Dma`-kind**
       grant through the per-task grant table, validates the constraint
       (`devres::dma_constraint`; rejects zero/over-max length), and
       carves a physically-contiguous, zeroed, coherent `RW` buffer bounded by
       the grant's `addr_limit` into the caller's own live space through the
       `devres::DmaAllocFacility` producer, returning the CPU-VA and copying
       the device-visible base out to a user pointer. The device-visible base
       is resolved by `devres::translate_device_addr`: the CPU-physical base
       for a coherent constraint, or — for a translating inbound viewport
       (`HwResource::dma_translated`, the Pi 4 PCIe `IB MEM 0x0..0x1ffffffff ->
       0x4_0000_0000` `dma-ranges`) — that base re-based onto the far side of
       the viewport, checked/fail-closed (§18.1). The guarded carve has one
       definition: `kernel/mem`'s
       borrowed `DmaWindowMap`, with `DmaPool` re-expressed as its owning
       wrapper (§2.2); `LiveSpace` gained `alloc_dma` + a DMA window and
       reclaims (zeroes + frees) every live DMA block on `Drop` at task exit
       (§4). Production producer `LiveDmaAlloc` installed for every port in
       `kernel_main`; the aarch64 `init_spawn`/`spawn_producer` thread a
       `spawn_layout::DMA_WINDOW` (3 GiB above the image bias). Host-tested
       (`kernel/mem` carve/addr-limit/Drop-reclaim, `devres` constraint,
       `dma_alloc` handler 6, `LiveDmaAlloc` producer, `abi-sys` marshalling)
       and proven on `-M virt` by the extended `mmio_map_qemu_aarch64` vertical
       (the EL0 program now also carves a `dma_alloc` buffer and round-trips a
       sentinel). New `tairix_rt::dma_alloc` + `tairix_sys_dma_alloc`; C header
       regenerated.
     - **5d — userland keyboard service** hosting the continuous report
       pump, autoloaded by `devmgr` over the 5d-0 surface, feeding the
       input-focus arbiter via `key_inject`. The "drivers in userland"
       steady state.
       - **5d-1 — the rt-backed `DriverHost` (`lib/drvrt`) — done
         (host-proven).** `tairix_drvrt::RtDriverHost` is the user-space
         analogue of the in-kernel keyboard service's `IdentityMmioMapper` +
         frame-allocator DMA host: it implements `DriverHost` + `MmioMapper` +
         `VirtioHost` over a fixed table of kernel-issued device-resource
         grants (`GrantedResource` = handle + `HwResource`). `map_window`
         resolves a requested `(phys,len)` to the covering grant, maps that
         grant's window once via the `mmio_map` syscall (cached), and
         translates an outbound `BusWindow` BAR pcie→cpu (§18.1);
         `alloc_dma_zeroed` carves the device-shared region via `dma_alloc`
         and mints a `DmaSlab` (optional caller-supplied non-coherent
         `SlabCoherencyFn`, never synthesised here — §2.20). Syscalls sit
         behind the host-testable `GrantSyscalls` seam (production
         `RtGrantSyscalls` → `tairix_rt`, §2.2); adds no authority, every
         check kernel-side, fail-closed (§4/§5.4/§2.9), allocation-free
         (`MAX_GRANTS`). 18 host tests; §3 + `SUMMARY.md` + `docs/src/lib/
         drvrt.md`. No production consumer yet (that is 5d-2), so no
         metal/virt step.
       - **5d-2-i — the `resource_grants` grant-delivery syscall — done
         (host-proven).** New `abi-v1` syscall **`resource_grants`** (no. 28,
         **no capability** — a task reads only its own grants, the §16.6/§24.3
         own-process baseline; unaudited) serialises the calling task's minted
         grant set from the per-task `AddressSpaceRegistry` grant table as
         consecutive `tairix_abi::hwtree::GrantedResource` records (handle +
         `HwResource`, `WIRE_LEN` = 40 — the one wire/owning definition,
         re-exported by `lib/drvrt`, §2.2), copies them out fail-closed
         (`BufferTooSmall` rather than a partial list, §2.9; `0` for an unbound
         task, §18.4). `AddressSpaceRegistry::grants_to_le_bytes` serialises;
         `RtDriverHost::from_grants_query` is the production constructor that
         issues the syscall and builds the grant table. New
         `tairix_rt::resource_grants` + `tairix_sys_resource_grants`; C header
         regenerated. Host-tested (abi round-trip/decode-reject, 5 kernel-core
         handler tests, 4 drvrt builder tests, abi-sys marshal). No
         metal/virt step (no production grant-minter / consumer yet — 5d-2-ii).
       - **5d-2-ii (a) — the production driver-spawn grant minter — done
         (host-proven + `-M virt`).** `KernelSpawnCtx` carries a kernel-sourced
         `grants: &[HwResource]` (the matched node's requested resources, never
         an untrusted caller — §4); `admit_process` mints one owner-checked,
         monotonic grant per resource for the freshly admitted child via
         `AddressSpaceRegistry::mint_grant`, reclaimed on exit. The ordinary
         `spawn` syscall passes an empty slice (a user task grants no device
         windows, §4/§5.2). Host-tested in kernel/core (mint-per-resource,
         owner-check, `GrantedResource` serialisation, empty-grant user-spawn)
         and proven on `-M virt` by the extended `driver_spawn_qemu_aarch64`
         vertical (the stub, spawned through the production `KernelSpawnCtx`/
         `spawn_with` with a granted MMIO window, enumerates it via
         `resource_grants` and refuses to reply on any shortfall). No
         `lib/abi`/C-header change.
       - **5d-2-ii (b-1) — the `devmgr`-driven driver-spawn path — done
         (host-proven + `-M virt`).** The device manager now sources the
         **matched node's** `HwResource`s into the spawn: `DriverLoader::load`
         gained a `resources: &[HwResource]` argument (`DeviceManager::autoload`
         forwards `HwNode::resources`), realising §18.3 — *a loaded driver
         receives only the resources its matched node requested.* The
         production loader is `kernel/tairix-kernel::driver_spawn_loader::
         SpawnDriverLoader` (impl `devmgr::DriverLoader`): it runs the signed
         `drvhost::Host::load` gate on the discovered `kind = UserSpace` image
         and spawns the verified payload through the architecture
         `DriverProcessSpawn` seam, threading those resources into
         `KernelSpawnCtx.grants` (the 5d-2-ii(a) minter). The gate/threading
         logic is host-tested with a recording `DriverProcessSpawn`; the full
         `autoload`→signed-gate→spawn→grant-delivery path is proven on `-M virt`
         by the extended `driver_spawn_qemu_aarch64` vertical (a discovered
         virtio node carries the MMIO resource the stub reads back via
         `resource_grants`; `13001` node-bound + `4302` PASS). **Security
         hardening (in place, §2.17):** the `drvhost` manifest signature now
         covers the **payload** as well as the header/caps/bind-table, so a
         spawned user-space driver's *program* is authenticated and cannot be
         substituted after signing — closing an unsigned-code-execution hole the
         spawned-driver path would otherwise rely on (empty-payload in-kernel
         images are unaffected; regression test `tampered_payload_refused`).
       - **5d-2-ii (b-2-i) — `lib/usb` extraction (the arch-neutral USB stack) —
         done (host-proven + whole gate).** The §17.4 layering forbids a
         `drivers/*` or `userland/*` crate from depending on another `drivers/*`
         crate, so an arch-neutral user-space keyboard driver could not compose
         `drivers/bus/usb` (xHCI) with `drivers/input/usb_hid` (HID decode) while
         the xHCI protocol lived inside the bus driver. The bus-agnostic xHCI
         protocol — the `XhciHost` register seam, the `Xhci` controller engine,
         the TRB/ring vocabulary, and the `UsbDevice` enumeration engine (since
         DEVICES.md multi-device: a table of concurrently served devices)
         — therefore moved into a new `lib/usb` (`tairix-usb`)
         crate (`lib/abi`-only, `no_std`, Tier-1-portable), the USB analogue of
         `lib/virtio` ↔ `drivers/bus/virtio` (§2.2/§6/§17.4). `drivers/bus/usb`
         keeps only the §8 `register` entry, the §18.3 `BIND_KEYS` table, and the
         PCI BAR/DMA `wiring` over `tairix_usb`; the kernel scaffold and `wiring`
         repoint to `tairix_usb::{Xhci, device::*, regs}`. The 81 USB tests split
         with the code (71 protocol in `lib/usb` + 10 driver `register`/bind/
         wiring), and the whole gate is green. Now a future keyboard-driver
         *process* (and any other host-controller/HID driver) can build on
         `lib/usb` without a driver→driver edge.
       - **5d-2-ii (b-2-ii) — generic boot-keyboard orchestration + shared
         `Delay` seam — done (host-proven).** The arch-neutral
         root→hub→downstream-HID bring-up is now one definition,
         `tairix_usb::device::UsbDevice::enumerate_boot_keyboard(delay)` in
         `lib/usb` (§2.2/§18) — enumerate the first connected root-hub port and,
         when it is a hub, power/settle/find/reset/settle and address the device
         on a second slot, discovered and fail-closed. Its timed settles use the
         microsecond `Delay` seam, hoisted from `drivers/bus/pcie_brcm` into
         `lib/abi` (`tairix_abi::Delay`) so the PCIe and USB driver crates share
         one trait (`pcie_brcm` re-exports it; a trait, so no C-header change).
         The in-kernel `keyboard_service` scaffold's `bring_up_keyboard` now
         calls the shared routine; its duplicated hub-descent helpers
         (`log_hub_ports`/`address_downstream_keyboard`/`log_downstream_keyboard`)
         and the `4127`/`4128` event-ids are deleted (§2.2/§2.14). Host-proven
         (`lib/usb` 74 tests + the new `enumerate_boot_keyboard_*` cases; kernel
         lib tests green). Touches the metal-confirmed scaffold bring-up
         (behaviour-equivalent) ⇒ operator §0.9 metal parity re-verify.
       - **5d-2-ii (b-2-ii) — arch-neutral boot-keyboard orchestration — done
         (host-proven).** `drivers/input/usb_hid::service::bring_up_boot_keyboard`
         is the composition the user-space keyboard driver runs at start-up: over
         its `DriverHost` (the rt-backed host built from its kernel-issued
         grants) it builds the growable `tairix_usb::SlabBank` over its host's
         DMA seam with the discovered aperture top (every chunk is
         aperture-checked at allocation time, fail closed, §5.4), maps its
         granted xHCI register BAR, brings the controller up
         (`tairix_usb::Xhci::open` + `UsbDevice::start`, growing the
         geometry-sized shared chunk — the one engine definition in `lib/usb`,
         §2.2), and runs the
         arch-neutral `enumerate_boot_keyboard`, returning a `BootKeyboard` the
         service loop drives with `pump_once`. It names no PCI/board (§2.20): the
         board PCIe root-complex bring-up + BAR assignment stay in the separate
         board bus driver, and the keyboard node is granted only its
         already-assigned BAR + a DMA constraint (§18.3). `usb_hid` now depends on
         `lib/usb` (a lib, §17.4). Host-proven (6 `service` tests: the
         cap-missing / no-mapper / no-DMA refusals, a DMA carve above the
         aperture and a DMA-alloc failure refused, and the all-valid path
         reaching the controller hand-off where the inert mock window faults
         `DeviceFault` — the metal boundary, mirroring `bus_usb`'s `wiring`
         tests). No `lib/abi`/C-header change; whole gate green.
       - **5d-2-ii (b-2-iii)** the `devmgr`-autoloaded keyboard driver `rxe`.
         - **`lib/hid` extraction + the driver binary — done (host-proven).**
           The reusable HID logic moved into a new **`lib/hid` (`tairix-hid`)**
           crate — the decoders, the console producer, and the xHCI boot-keyboard
           orchestration (`bring_up_boot_keyboard`, `derive_keyboard_resources`,
           `KeyboardResources`) — the USB analogue of `lib/usb` ↔
           `drivers/bus/usb` (§2.2 / §6 / §17.4); `drivers/input/usb_hid` shrank
           to the §8 `register` + `BIND_KEYS` identity. The new
           **`drivers/input/usb_kbd` (`tairix-drv-input-usb-kbd`)** binary is the
           user-space keyboard driver: a pure-Rust `tairix-rt` program depending
           only on `lib/*` (hid/drvrt/rt/caps/abi — so §17.4 holds and the kernel
           never links `tairix-rt`) that builds `RtDriverHost::from_grants_query`
           over its kernel-issued grants (coherency `None` — kernel-coherent DMA,
           §2.20), derives its BAR + DMA aperture from the same grants with the
           host-tested `tairix_hid::derive_keyboard_resources` over the new
           `RtDriverHost::resources()` accessor (no second `resource_grants`
           syscall, §2.16), runs `bring_up_boot_keyboard`, then loops `pump_once`
           with a `KeyInjectSink` over `key_inject` + the userland `ClockDelay`,
           yielding between polls (§2.1). Fail-closed exit codes (§2.9); every
           capability + bound re-checked kernel-side (§5.4). Host-proven
           (`tairix-hid` 45, `usb_hid` 4, `drvrt` 24); usb_kbd + the aarch64
           kernel build freestanding on all three Tier-1 targets. No
           `lib/abi`/C-header change. AGENTS.md §3 + SUMMARY.md gained `lib/hid`.
           Docs: `docs/src/lib/hid.md`, `docs/src/drivers/input.md`,
           `docs/src/lib/drvrt.md`, the crate READMEs.
         - **Signed-store candidate scan — done (host-proven).** The §18.3 /
           §18.6 store scan that turns the installed `/System/Drivers/` bundles
           into autoload candidates is `tairix_drvhost::store`
           (`scan_store(source, paths, sink) -> DriverStore`): it reads each
           enumerated bundle through the existing `ImageSource`, parses the
           `.rxe` manifest with the same `ParsedImage` splitter the load gate
           uses (no drift, §2.2), decodes the bind table fail-closed, and emits
           owned `ScannedDriver`s whose `DriverStore::candidates()` lends the
           canonical `tairix_devmatch::DriverCandidate` slice
           `DeviceManager::autoload` consumes. A match step only — no authority,
           no signature check (that stays at `Host::load` when a candidate wins
           a node, §18.6); a malformed/unreadable bundle is skipped + logged
           (events 7030 accept / 7031 skip), never fatal (§18.4/§5.4). drvhost
           gained a `lib/devmatch` dep (lib/* only, §17.4). Host-proven (8
           `store::tests`); no `lib/abi`/C-header change. Docs:
           `docs/src/drivers/host.md` ("Signed-store scan").
         - **`/System/Drivers/` store enumeration (kernel half) — done
           (host-proven).** `tairix_kernel_core::driver_store::enumerate_driver_store`
           is the boot-time walk that turns the on-disk store tree into the
           bundle image-path list `scan_store` consumes. Mirroring
           `users::load_users_db`, it builds the shared root-backed VFS
           (`crate::fs::root_backed_vfs` — the private-root-mount handle + minimal
           `Vfs` builder hoisted out of `users.rs`, §2.2) and walks
           `/System/Drivers/` through the §5.3-checked per-inode delegation under
           the uid-0 bootstrap identity (no §5.1 bypass), collecting every
           regular file's path. It is structural path discovery only — it never
           reads, parses, or trusts a bundle (the load gate does, §18.6); the
           walk is bounded (`MAX_STORE_DEPTH` / `MAX_STORE_DRIVERS`, §24.4) and
           fail-closed: a missing store, an unreadable sub-directory, or a
           malformed entry simply contributes fewer paths, never aborts the boot
           (§18.4/§2.9). One audit record `DriverStoreScanned` (event 4042,
           `drivers`/`skipped` counts). Host-proven (7 `driver_store` tests via a
           tree mock fs); no `lib/abi`/C-header change. Docs:
           `docs/src/architecture/kernel.md` (audit catalogue).
         - **VFS-backed `ImageSource` (the bundle-byte reader) — done
           (host-proven).** The enumeration yields the bundle *paths*; this reads
           their *bytes*. `tairix_kernel_core::driver_store::DriverImageReader`
           builds the shared root-backed VFS once (§2.16); `read_image` reads one
           bundle off the mounted root under the uid-0 bootstrap identity —
           path-within-`/System/Drivers/`, `MAX_DRIVER_IMAGE_LEN` (16 MiB §24.4)
           bound *before* reading, full read, **appends** to the caller buffer,
           fail-closed/`buf`-untouched on refusal (§5.4/§2.9), `DriverImageError`
           →`Errno`. Since §17.4 forbids a `kernel/core`→drvhost edge, the bin
           crate supplies the read-only `/System` file service
           `tairix_kernel::system_files::SystemFileService` (Design D D2b-1):
           one object over the mounted volume that both **lists** the store
           (`list_store`→`enumerate_driver_store`) and **reads** a bundle's
           bytes (an `ImageSource`→`DriverImageReader`), reader + root-volume
           driver behind a `RefCell` for the `&self`-vs-`&mut` bridge, adding
           no authority. Consolidating list + read behind this one seam (§2.2)
           is the seam the D2b-2 `/System` file-read `IPC_RECV` endpoint wraps.
           Host-proven (11 reader + 6 service tests); no `lib/abi`/C-header
           change. Docs: `docs/src/drivers/host.md` ("Reading the bundle bytes
           off the root volume").
         - **Boot-wiring composition — done (host-proven).**
           `tairix_kernel::driver_autoload::autoload_drivers` is the single
           production composition: it scans the store
           (`drvhost::store::scan_store` over the `SystemFileService`
           `ImageSource` and its `list_store` paths — a match-only step,
           §18.6), runs
           `devmgr::DeviceManager::autoload`, and loads each winner through
           `driver_spawn_loader::SpawnDriverLoader` (signed gate → process
           spawn with exactly the matched node's resource grants, §18.3),
           taking the spawn mechanism behind the `DriverProcessSpawn` seam so it
           stays scheduler-agnostic (§17.1). Host-proven (5 tests: signed-match
           spawn with the node's resources, untrusted-signature/missing-cap
           fail-closed, unmatched unbound, empty-store). Docs:
           `docs/src/drivers/host.md` ("Autoloading by discovery").
         - **Mounted-root composition — done (host-proven).**
           `tairix_kernel::driver_autoload::autoload_from_mounted_root(fs, …)`
           is the thin glue that drives `autoload_drivers` straight off a
           mounted root volume `fs`: it opens one
           `SystemFileService::open(fs, …)`, lists the store with
           `service.list_store(…)`, then reads each winning bundle's bytes back
           through the *same* service (the one `&mut fs` borrow's list-then-read
           is sequential, so it never overlaps), and defers to
           `autoload_drivers`. It
           adds no policy and fails closed (`VfsError`) only if the private root
           mount cannot be built; a missing/empty/malformed store binds nothing
           in `Ok` (§18.4/§2.9). Host-proven (3 `driver_autoload` tests over the
           shared `MockRootFs` fixture: discovered-bundle spawn, empty store,
           untrusted bundle fail-closed). Docs: `docs/src/drivers/host.md`.
         - **Scheduler-agnostic driver-spawn seam — done (host-proven + `-M
           virt`).** `InitSpawnCtx::spawn_driver_process(spawn, path, rxe,
           caps, grants, args, node_id)` (default fail-closed
           `NotImplemented`, §2.9) builds
           the live `KernelSpawnCtx` inside kernel/core's now-public,
           constructible `KernelInitSpawner` (grants minted owner-checked §18.3;
           driver established `DescriptorTable::closed` §20; supervisor
           `SecTaskId(0)`; the real `ProcessWait` threaded out of `run_phases`)
           and drives the arch `ProcessSpawn::spawn_with`, so the bin crate
           names neither the feature-selected scheduler nor `KernelSpawnCtx`
           (§17.1). The bin-crate
           `driver_spawn_loader::InitCtxDriverProcessSpawn` is the
           `DriverProcessSpawn` bridge (`&dyn InitSpawnCtx` + the arch
           `&dyn ProcessSpawn`) `SpawnDriverLoader` reaches it through.
           Host-proven (kernel/core default-fail-closed + delegation-to-a-
           recording-producer; bin-crate adapter forwards
           payload/caps/grants/args unchanged), and the
           `driver_spawn_qemu_aarch64` `-M virt` vertical now drives the chain
           through this seam (no hand-built `KernelSpawnCtx`). No
           `lib/abi`/C-header change.
         - **Boot-path attachment of the composition — done (host-proven +
           `-M virt`).** The in-kernel unlock kthread now runs the autoload
           composition off the just-mounted root. Landed: the kernel/core
           **`'static`-spawner seam** (`InitSpawn::spawn_init` takes
           `&'static (dyn InitSpawnCtx + Sync)`, `kernel_main` leaks the ctx,
           forwarded through the three arch `init_spawn` seams to
           `unlock_service::spawn_if_present`); the **discovered-hardware-tree
           stash** (`audit_root_storage_binding` leaks the full
           `&'static [HwNode]` tree — virtio-MMIO block child probed in — and
           `unlock_service::record_boot` stashes it beside the binding); and
           the unlock-kthread **tail call** (`aarch64::root_unlock::run_unlock`
           builds the arch-neutral `unlock_service::AutoloadHook` over the
           stashed tree + the leaked `'static` ctx and hands it to
           `unlock_root_disk_interactively` as the `MountedRootHook`, which
           calls `autoload_from_mounted_root` the instant the root mounts;
           empty store binds nothing in `Ok`, §18.4). The autoloadable input
           driver and its `-M virt` autoload vertical are **done** (host-proven
           + `-M virt`):
           - the autoloadable **user-space input driver `rxe`** — done.
             `drivers/input/virtio_kbd` (`tairix-drv-input-virtio-kbd`) is a
             freestanding `tairix-rt` program (lib/* only:
             virtio/virtio_input/drvrt/rt/caps/abi, §17.4) whose signed manifest
             carries the §18.3 `BIND_KEYS` (`HwMatchKey::virtio(18)`,
             `VIRTIO_INPUT_DEVICE_ID = 18`, exact-match tier); it builds
             `RtDriverHost::from_grants_query` over its grants, maps its sole
             register window, builds `MmioTransport`, runs `VirtioInput::open`,
             and loops `poll` → `VirtioKeyboardConsole::feed` → `key_inject`.
             The reusable `open`/`poll`/`decode` logic lives in `lib/virtio_input`
             and the concrete `MmioTransport` in `lib/virtio` (§2.2/§17.4). The
             metal Pi keyboard stays `usb_kbd`, flipped at 5e. **Now
             interrupt-driven (not a busy poll, §2.1/§2.16):** the discovered
             input node also carries its GICv2 IRQ line (`HwResource::irq`, the
             INTID from `device_spi`, §18.1); the kernel re-arms it on the
             driver's behalf via the arch-neutral `IrqController::rearm`
             (route-to-CPU + unmask) driven from the `irq_wait` park path; and
             `RtDriverHost::notify_wait` binds the line once and `irq_wait`s. The
             driver/`autoload_caps`/manifest carry `CAP_IRQ_BIND`. **Root-cause
             fix that made the vertical green:** `lib/virtio::SplitQueue` was
             missing its virtio 1.1 §2.7.13.3 memory barriers (a defect the
             synchronous virtio-blk path tolerated but the asynchronous
             virtio-input device exposed as a stale/empty avail ring); added a
             `fence(Release)` before publishing avail.idx, `fence(SeqCst)` before
             `notify`, and `fence(Acquire)` after reading used.idx. Doc:
             `docs/src/drivers/virtio.md` "Virtqueue memory ordering";
           - **virtio-input hardware-tree discovery — done:**
             `hwdiscovery::observe_virtio_mmio_input_devices` probes each
             `virtio,mmio` slot for virtio-input (id 18) and emits a discovered
             `HwDeviceClass::Input` node carrying its register window
             (`HwResource::mmio(base, len)`, the extent from the new
             `VirtioMmioBus::slot_window`, §18.1) — the node the autoload spawn
             mints the user-space driver's window grant from (§18.3). Wired
             into `aarch64::boot` beside the block probe, host-tested,
             metal-neutral (no-op on the Pi tree, §2.17);
           - the **`-M virt` autoload vertical — done.**
             `tests/integration/autoload_input_qemu_aarch64` boots the
             production pipeline on `virt` with the shared encrypted-root
             whole-disk fixture, planted with the kernel-signed driver bundles
             the `image_drivers` pipeline cross-compiles and signs (the
             `virtio_kbd.rxe` at `/System/Drivers/input/virtio_kbd/Run`), and
             an attached
             `virtio-keyboard-device`: unlock → enumerate → match the discovered
             virtio-input node → verify against `KERNEL_DRIVER_SIGNER_PUBKEY` →
             spawn into a user-space process → a typed keystroke reaches the
             seat registry via `key_inject` (PASS on
             `AuditEvent::InputDelivered` `EventId(4050)`). The signing/witness
             prerequisites are landed (`KERNEL_DRIVER_SIGNING_SEED`
             single-sources both the kernel build and the fixture, §2.2; the
             one-shot `InputDelivered` witness is the
             `SeatRegistry::note_first_delivery` latch, carrying no key
             content/timing, §20/§23.1). The blocking-wait subsystem this and
             the interactive UART login depend on is landed: the freeing
             `tairix-kalloc` allocator (§4 deterministic OOM), `KernelProcessWait`
             and `BlockingConsoleRead` true parks (`PROCWAIT_WAITQ` /
             `CONSOLE_WAITQ`, §2.1), and a reliable device-IRQ-delivery path —
             console input is poll-backed (`UartConsoleRead::read` drains the
             PL011 FIFO from the reader's context before parking), the RX ISR
             clears-then-rechecks (no lost wakeup), and the non-preemptible
             dispatch loop runs an interrupt poll point (`KernelArch::poll_interrupts`)
             between steps so a device IRQ is taken promptly, not only at idle.
           **Remaining — re-scoped under design B (operator-approved).** The
           metal keyboard is needed to type the *encrypted-root* unlock
           passphrase, so it cannot be autoloaded from the encrypted root
           (chicken-and-egg). The correct §18 path puts the signed driver
           store on a **dedicated read-only, signed `/System` volume reachable
           before unlock** (per-bundle Ed25519 signatures make an unencrypted
           read-only store tamper-evident, §18.6; `/System` holds no secrets).
           Full architecture + the staged increments **B1–B5** live in
           `plans/PI.md` ("Pre-unlock signed driver store (design B)"). The
           in-kernel scaffold stays the metal keyboard driver and stays wired
           throughout, so the working metal keyboard never regresses (§2.17):
           - **B1 — DONE (host + `-M virt`)** — three-partition image (FAT boot
             + read-only `/System` + encrypted data root) in `tools/mkimage`
             (`build_system_partition` + `build_rpi_image`), a `ARXFSSystem`
             partition role in `lib/partition`, `ARXFS::open_read_only` + the
             non-secret `SYSTEM_VOLUME_KEY`, and the kernel mounting `/System`
             read-only over a `lib/partition` window in
             `root_mount::autoload_system_drivers` (audited 4140/4141). The
             `encrypted_root_image` fixture authors the split, with the
             autoload driver bundles built and planted by the `image_drivers`
             pipeline;
           - **B2 — DONE (host + `-M virt`)** — the aarch64 unlock kthread runs
             `root_mount::autoload_system_drivers(&mut blk, &mut AutoloadHook,
             audit)` **once, before** the passphrase prompt: it mounts the
             read-only `/System` volume and autoloads its signed store, so the
             keyboard comes up in user space before unlock. The encrypted-root
             post-mount hook is removed (the unlock fns are hookless,
             `NoMountedRootHook` deleted, §2.14); the store is addressed
             relative to the scanned volume's root via a `store_root` arg on
             `enumerate_driver_store`/`read_image`, the `/System` volume scanned
             at `SYSTEM_VOLUME_STORE_PATH` (`/Drivers`); fixtures plant the
             signed bundle into the `/System` volume. Proven by
             `autoload_input_qemu_aarch64` (PASS pre-unlock on `InputDelivered`);
           - **B3 — DONE (host + metal)** — floor USB→`hwtree` enumeration;
             the Pi 4 UART log shows the keyboard up at the init seam
             (`4129`/`4131`) and `devmgr` `13002` unbound records for the HID
             node;
           - **B4 — DONE (host; metal acceptance pending).** The aarch64
             unlock kthread dispatches on the bound floor block driver
             (`run_unlock` → `virtio_blk_unlock` / `emmc2_unlock`): the EMMC2
             arm admits `tairix-drv-storage-emmc2` through the signed §8 gate,
             maps the matched node's sole SDHCI register window under
             `CAP_MMIO_MAP` through a minimal in-kernel MMIO-only `Emmc2Host`,
             **discovers the controller's GIC SPI from the firmware device tree
             (`emmc2_spi`) and binds/routes/arms it on the published IRQ table**,
             and feeds the opened `Block` to the shared `finish_unlock` tail
             virtio-blk also uses (§2.2). The SD command/transfer completion
             waits **block on that bound line** through an `Emmc2Completion`
             `CompletionWait` over the same task-parking waiter the virtio path
             uses (`tairix_kernel_core::IrqParkWaiter`, §2.2): a syscall context
             parks its task off the run queue (woken by `irq_wake`) so the
             dispatch loop keeps running for the whole device wait, a
             boot-kthread context takes the bounded race-free `wfi` fallback,
             and a controller silent past the 2 s budget fails the transfer
             closed (`DeviceFault`) — never busy-spinning a status register and
             never halting the CPU under a running system (§17.1/§2.16/§2.23 —
             a device wait must not starve the dispatch loop or the serial
             drain during `/System` autoload). On a real
             Pi 4 it mounts `/System` (4140) and unlocks
             the encrypted root (4133 → users db 4040 → 4136). Two SD defects
             fixed to get there are the load-bearing facts of the driver:
             `reset_and_clock` powers the card rail (3.3 V via `CONTROL0`,
             Linux's `0x0F`) before clocking, and `geometry_from_csd` reads
             `CSD_STRUCTURE` at the right-aligned `RESP3[23:22]`
             (`(resp[3] >> 22) & 0x3`); both regression-tested. The
             `EventId(4139)` line carries `stage=`+`error=` for any future
             stall. Metal acceptance also surfaced — and this work fixed —
             two login defects: `login` cached a pre-unlock empty users
             database, and it printed `Username:` over the unlock kthread's
             `Root passphrase:` prompt on the shared console. Both are fixed
             by the `LateUsersDb` three-state seam (`WouldBlock` while the
             unlock is pending → `login` waits without prompting; the
             installed database, or `NotImplemented` once it resolves empty)
             plus `tairix_login::supervise` acting per round (P11). The
             unlock also tries the **blank** passphrase silently first
             (`finish_install` shared with the prompted path), so the
             installer image (`INSTALLER_PASSPHRASE` blank, §11) auto-unlocks
             with **no prompt**; only a non-blank passphrase (debug
             `DEBUG_PASSPHRASE` = `root`, or a production operator-chosen one)
             draws `Root passphrase:`. `build_rpi_image` derives the
             passphrase from the profile (`passphrase_for`), never a caller
             argument;
           - **B5 (= 5e)** — re-scoped under **DESIGN D** (reactive top-down
             discovery); see item 5e below and `.junie/next-pi-prompt.md`.
     - **Increment C (B5 prerequisite) — DONE (metal-confirmed) — ported the
       autonomous floor bring-up off the in-kernel scaffold onto the
       `lib/abi::DriverHost` contract.** A blind B5 flip would have bricked the
       metal keyboard: the autonomous VL805 bring-up
       (PCIe train + VideoCore firmware reload + xHCI bring-up + enumeration +
       HID-node emission) lives **only** in `usb_keyboard.rs`/`keyboard_service.rs`,
       and the floor `drivers/bus/*` crates expose just `register()`. The fix
       (operator-approved option C-0) extends the host contract so the floor
       driver can run that bring-up talking **only** through `lib/abi`
       (§17.4) — no `kernel/*` edge — then relocates the orchestration there,
       keeping the in-kernel scaffold pump live throughout (§2.17).
       - **C-1 — `DriverHost` contract extension — DONE (host-proven).** The
         host surface the floor bring-up needs is landed in `lib/abi`: a
         bus-neutral `DmaHost` trait (`alloc_dma_zeroed`) with `VirtioHost:
         DmaHost` so a non-virtio bus driver allocates DMA without a
         virtio-shaped trait and the contract is defined once (§2.2); the
         board-neutral `MailboxChannel` seam + `MAILBOX_PROPERTY_WORDS` (the
         VideoCore property width `lib/vcmailbox` now re-uses, §2.2) so a driver
         runs a firmware exchange with the doorbell/buffer/translation owned by
         the host (board specifics behind `lib/vcmailbox`, §2.20); and
         `DriverHost::dma_host()`/`mailbox()`/`emit_node(HwNode)` accessors
         (default `None`/`Unsupported`, mirroring the `virtio_host()`/
         `mmio_mapper()` extension pattern). `RtDriverHost` exposes `dma_host()`,
         and `lib/hid`'s user-space keyboard bring-up now carves its xHCI DMA
         through `dma_host()` (a USB device is not virtio). Host-proven (lib/abi
         contract-surface tests; all virtio hosts split into `DmaHost`+`VirtioHost`;
         `lib/hid`/`lib/drvrt`/`lib/virtio` + the virtio-driver crates green); no
         `#[repr(C)]`/syscall/error/cap change ⇒ no C-header drift. Docs:
         `docs/src/abi/driver_traits.md`.
       - **C-2 — relocate the autonomous bring-up into the floor `drivers/bus/*`,
         each driver strictly its own device with no drivers→drivers edge.** The
         three concerns are three separate driver crates (operator-directed; the
         earlier "VL805 reload lives in `pcie_brcm`" wording was wrong): the
         board-specific PCIe-RC train + config-scan + BAR-assign in
         `drivers/bus/pcie_brcm`; the board-neutral xHCI bring-up + enumeration +
         HID-node `emit_node()` in `drivers/bus/usb/xhci`; and the VL805-specific
         VideoCore-mailbox firmware reload in its own `drivers/bus/usb/vl805`
         device crate (§2.20 — it must **not** leak into the generic PCIe or USB
         layer). Each consumes only `lib/abi`/`lib/*` through `DriverHost`
         (§17.4); the hwtree decouples them (§18.1).
         - **Driver structure / VL805 reload — DONE (host-proven; metal-confirmed).**
           The generic xHCI driver moved
           `drivers/bus/usb` → `drivers/bus/usb/xhci` (package name unchanged);
           the new `drivers/bus/usb/vl805` crate owns the firmware-reset
           vocabulary (`FirmwareResetOutcome`/`FirmwareResetFailure`,
           `VL805_FIRMWARE_DEV_ADDR`, the §18.3 `BIND_KEYS` for PCI `1106:3483`)
           and the `probe_firmware_revision`/`reload_firmware` policy run over the
           C-1 `MailboxChannel` seam, reusing the `lib/vcmailbox` property layout
           (§2.2, never re-derived). The kernel keyboard composition supplies a
           `KernelMailboxChannel` that owns the `MmioMailbox`
           (doorbell/buffer/coherency mechanism) and logs the `4121` exchange
           diagnostics; the floor xHCI bring-up runs `vl805::reload_firmware`
           over it through `host.mailbox()`. 7 vl805 host tests (mock-firmware
           channel); kernel host + aarch64 freestanding green; scaffold pump
           unchanged (§2.17). No `#[repr(C)]`/syscall/lib-abi change ⇒ no
           C-header drift.
         - **USB autonomous half — DONE (host-proven).** `tairix_drv_bus_usb::`
           `wiring::bring_up_boot_input` maps the controller BAR
           (`mmio_mapper()`), carves DMA via the bus-neutral `dma_host()`, brings
           the controller up, enumerates the boot device, augments the HID
           `HwNode` with its xHCI-BAR (`HwResource::mmio`) + DMA
           (`HwResource::dma`) grants, and `emit_node()`s it. Host tests prove the
           composition + fail-closed paths to the controller hand-off; the live
           enumerate→emit is the metal item.
         - **PCIe autonomous half — DONE (host-proven).** `drivers/bus/pcie_brcm`
           owns its discovered-node parsing beside the link-training engine it
           feeds (`AGENTS.md` §2.2 / §2.21): `wiring::pcie_bringup_from_node`
           reads the controller window + inbound/outbound address windows off the
           `brcm,bcm2711-pcie` `HwNode` into a `PcieBringup` (fail-closed
           `BringupError` per missing resource, §18.5), and the §18.6 autonomous
           `wiring::bring_up_from_node` maps the window under `CAP_MMIO_MAP` and
           trains the link (`DriverError::NotFound` on an incomplete node). The
           types moved out of the kernel scaffold (`usb_keyboard.rs`), which now
           re-exports `PcieBringup` and consumes the relocated parse (§2.14); the
           scaffold pump stays the live keyboard (§2.17). 8 pcie_brcm host tests;
           no `lib/abi`/`#[repr(C)]` change ⇒ no C-header drift.
         - **Kernel autonomous sequencing — DONE (host-proven; metal-confirmed).**
           The in-kernel `bring_up_keyboard` composition
           now sequences the floor crates over the `DriverHost` contract —
           `pcie_brcm::wiring::open_discovered` trains the link,
           `vl805::reload_firmware` runs over `host.mailbox()`, and
           `tairix_drv_bus_usb::wiring::bring_up_boot_input` maps the BAR, carves
           DMA, enumerates the boot keyboard, and publishes it via
           `host.emit_node()` (forwarded to the boot hardware tree by the
           in-kernel `KernelBootTreeEmitter`) carrying its xHCI-BAR + DMA grants.
           The bespoke in-kernel xHCI/firmware-version diagnostics
           (`open_controller`, `wait_for_caps_ready`, `VideoCoreFirmwareReset`,
           events `4102`/`4104`/`4106`/`4107`/`4109`/`4110`/`4114`/`4118`/`4122`/
           `4123`/`4124`/`4125`/`4126`) are deleted (§2.14); `spawn_pump` stays
           the live keyboard until B5 (§2.17). Kernel host lib + aarch64
           freestanding green, and metal-confirmed on a real Pi 4B: the floor
           chain trains PCIe, reloads VL805 firmware over `host.mailbox()`,
           enumerates the boot keyboard, and emits a *bindable* HID node carrying
           its xHCI-BAR + DMA grants, with the scaffold pump still delivering
           keystrokes and typed `root` login working. (`raspi4b`/QEMU cannot
           model the VL805, so this was metal-only-verifiable.) Increment C is
           therefore complete; B5 (5e below) is the next increment.
     - **5e — re-scoped under DESIGN D (operator-approved option A): full
       reactive, top-down driver discovery.** The narrow "flip the keyboard onto
       autoload and delete the scaffold" is rejected: `usb_keyboard.rs` does the
       bring-up *the wrong way around* (a leaf "keyboard" file that trains PCIe,
       reloads VL805 firmware, brings up xHCI and enumerates). The correct §18
       shape is top-down — core bus discovery → each node discovers its children
       and autoloads the matching driver, down to a `usb_kbd` that does **zero**
       orchestration — and **reactive**, so it also serves USB hotplug
       (attach/detach). The VL805 firmware-reload mailbox is served by an IPC
       `vcmailbox` driver (capability-gated endpoint, no cross-device ambient
       grant, §4/§2.20). Full architecture + the staged increments **D1–D5** live
       in `.junie/next-pi-prompt.md` ("DESIGN D"). The in-kernel scaffold stays
       the live metal keyboard and stays wired until the **D5** atomic flip, so
       the working keyboard never regresses (§2.17); D5 is metal-only-verifiable.
       - **U-MSI — interrupt-driven USB keyboard over BCM2711 PCIe MSI — DONE
         (host-proven + whole gate; metal-confirmation pending, §0.4).**
         Supersedes every busy-poll/keep-alive workaround (§2.23). The VL805
         xHCI completion is now an interrupt the keyboard driver parks on: the
         VL805 raises an MSI write, the BCM2711 PCIe root complex's internal MSI
         controller demultiplexes it onto its one shared GIC SPI, a kernel
         chained handler fans it out to a per-vector virtual IRQ line, and
         user-space `usb_kbd` `irq_wait`s on that line over the proven
         `irq_bind`/`irq_wait` + re-arm path. No kernel/irq core change was
         needed — a composite line-range `IrqController` routes GIC INTIDs vs.
         MSI virtual lines. The end-to-end path is:
         - `lib/usb`: `Xhci::enable_interrupter` (IMAN.IE / IMOD=0 /
           USBCMD.INTE) + `acknowledge_interrupt` (clear IMAN.IP, keep IE),
           surfaced on `UsbDevice` and reached through `BootKeyboard::source_mut`.
         - `lib/pci`: `route_msi(bdf, MsiMessage)` on the `PciBus` seam (program
           the legacy MSI capability: Message-Address lo/hi + Data, MSI Enable,
           Multiple-Message-Enable forced to one vector; fail closed on a
           64-bit doorbell against a 32-bit-only capability).
         - `kernel/arch/aarch64/src/brcm_msi.rs`: the BCM2711 RC MSI register
           driver (doorbell/data-config programming, per-vector
           mask/unmask/clear, `pending`/`pending_vectors` demux, `msi_message`
           builder) over an `MsiMmio` seam; freestanding `VolatileMsiMmio` over
           the discovered RC base. Constants (`0x4044`/`0x4048`/`0x404c`, INTR2
           `0x4500`, target `0xFFFF_FFFC`, data magic `0x6540`) from Linux
           `pcie-brcmstb.c` + the BCM2711 datasheet, isolated for metal check.
         - `kernel/tairix-kernel/src/aarch64/gic_irq.rs`: the `'static`
           `CompositeIrqController` (a line in `[MSI_LINE_BASE, MSI_LINE_TOP]`
           routes to the brcm MSI controller, else the GIC), the lazy
           `BrcmMsi` bring-up + free-vector bitmap allocator
           (`allocate_msi_vector`), the `BrcmMsiAllocFacility`, and the chained
           demux in `production_device_irq_dispatch` (read `pending`, fire each
           vector's virtual line — mask-before-wake via the composite — clear
           its INTR2 status, then `irq_wake`). `gic_irq_routing` now returns the
           composite controller with `MSI_LINE_TOP` as the bind ceiling.
         - `msi_alloc` syscall (`abi-v1` #39, `CAP_IRQ_BIND`, `MsiAllocation`
           out-record): the `MsiAllocFacility` kernel seam installed via
           `KernelArch::msi_alloc_facility`; the handler allocs a vector, mints
           the caller an `HwResource::irq` grant for the virtual line, and
           copies the doorbell out. Wrappers in `lib/rt`/`lib/abi-sys`/`lib/drvrt`
           (`DriverHost::alloc_msi`).
         - `boot.rs` configures the RC base (`brcm_msi::configure`) and records
           the MSI GIC SPI (the `brcm,bcm2711-pcie` node's 2nd `interrupts`
           entry, `pcie_msi_spi`) post-MMU.
         - `pcie_brcm::publish_usb_function` allocs+routes MSI (best-effort) and
           forwards `HwResource::irq(line)` on the VL805 node; `vl805::
           build_xhci_node` forwards it onto the xHCI node; `usb_kbd` enables
           the interrupter, `irq_bind`s the granted line, and runs
           `loop { irq_wait; acknowledge_interrupt; drain }`, falling back to the
           bounded poll loop only when no IRQ grant is present (no MSI board).
           `usb_kbd`'s bundle manifest carries `CAP_IRQ_BIND`.
         - **Metal confirmation pending:** QEMU models no Pi PCIe/USB/MSI
           (§0.4), so the doorbell offsets/magic and the end-to-end MSI delivery
           are verified on the host (unit tests + the whole gate) and need an
           on-metal re-test of the HDMI keyboard across a post-activity idle.
           If a vector's offsets prove wrong on metal, only `brcm_msi.rs`'s
           isolated constants change.
       - **D1 — runtime hardware-inventory store — DONE (host-proven + whole
         gate).** `kernel/tairix-kernel::hwtree_store::HwTreeStore` (`seed` /
         `append` / `snapshot`, growable §24.1) is the single authoritative
         discovered-hardware inventory (§18.1/§2.2), replacing the
         leak-a-new-`&'static`-slice stash in `unlock_service`: `record_boot`
         seeds it, a user-space bus driver appends discovered children at
         runtime through `hw_emit_node` (`publish_child`), and every reader —
         `hw_tree_read`/`hw_tree_wait` and the driver-store load gate's
         matched-node grant resolution (`resolve_resources`) — reads the one
         live store directly, never a frozen snapshot (§18.4). The generation
         counter +
         `hw_tree_wait`, node removal, and the `hw_*` syscalls are deferred to
         D2/D4 with their first user-space consumers (§2.3/§2.4). Gate: `cargo
         fmt --all --check`, `cargo xtask ci` (both Pi images built, no
         ABI/C-header drift), `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both
         --secs 20` all green.
       - **D2 — user-space reactive `devmgr` service.** Move the match loop into
         a long-running `userland/system/devmgr` binary (read tree → match store
         → load/spawn winners → block on `hw_tree_wait` → unload on
         `hw_remove_node`); `init` spawns it after `/System` mount; delete the
         in-kernel single-pass `driver_autoload` it subsumes (§2.14). Adds
         `hw_tree_read`/`hw_tree_wait` + the `driver_store_load` syscall with this
         consumer.
         - **Confirmed contract (operator-approved).** Mechanism stays in the
           kernel TCB; only *policy* (matching) moves to user space (§4). The
           kernel keeps signature verification, bundle bytes, and process spawn;
           `devmgr` reads the discovered tree (`hw_tree_read`, gated by the
           existing `CAP_SYSINFO_HW`), reads the **already-fail-closed-parsed
           store catalogue** (opaque kernel-issued bundle ids + decoded bind
           keys, *no* bytes), matches node→bundle with the shared `lib/devmatch`,
           and calls `driver_store_load(bundle_id, node_id)` (gated by
           `CAP_DRV_LOAD`). The kernel re-runs the full signed §8 gate over the
           named bundle and mints exactly that node's resource grants. **No
           general user-space VFS-read syscall is introduced** (smallest new
           attack surface, §5.4/§23.1). `hw_tree_wait` blocks on the store's
           monotonic generation counter, mirroring the `irq_bind`/`irq_wait`
           park.
         - **`/System` stays mounted for life (operator decision — option X).**
           The driver store must be reachable *after* boot for on-demand and
           reactive (hotplug) loads, and `/System` must stay mounted anyway so
           other subsystems can reach it. So a kernel block-device **sharing
           layer** lets the one whole-disk device back two concurrent partition
           windows — a persistent read-only `/System` mount owned by a
           kernel-resident `DriverStoreService` **and** the encrypted-root unlock
           window — with `lib/sync` serialisation (§4 SMP). This replaces the
           current "borrow `/System` then move the one disk into the unlock"
           ownership in `finish_unlock`, so it **modifies the metal-confirmed
           Design-B unlock path** and carries a §0.9 metal re-verification of the
           Pi unlock.
         - **Staging (§2.3 — syscalls land only with their user-space consumer).**
           - **D2a — shareable block layer + persistent `/System` mount +
             kernel-resident `DriverStoreService`.** The §2.19 prerequisite, and
             independently valuable (other subsystems can now reach `/System`).
             No new syscalls; the *existing* in-kernel autoload consumes the
             persistent service (same match+spawn result, proven by
             `autoload_input_qemu_aarch64`), so there is no behaviour change
             beyond `/System` now staying mounted. Metal: re-verify the Pi
             unlock still mounts the root and logs in (§0.9). Split for safe
             metal-verified landing of a metal-confirmed boot path (operator
             pre-agreed metal re-verify between chunks):
             - **D2a-1 — kernel block-sharing layer — DONE (host-proven + whole
               gate).** `kernel/tairix-kernel::shared_block`: `SharedBlock<B>`
               wraps the one brought-up disk behind a `lib/sync::SpinLock` and
               hands out `SharedBlockHandle`s (each itself a `Block`), serialising
               every device op (§4 SMP) with a once-cached, lock-free
               `geometry()` (§2.16) and a fail-closed geometry-fault refusal
               (§2.9). The aarch64 `finish_unlock` now wraps `blk` once and drives
               the `/System` autoload window and the encrypted-root unlock window
               through **two concurrent serialised handles**, replacing the
               borrow-then-move of one device — the concurrent-windows-over-one-
               disk ownership model D2a-2's persistent mount is built on. No
               device-backing lifetime change yet, so the metal risk is the small,
               reviewable routing change; the virtio path is exercised by the
               `autoload_input_qemu_aarch64` vertical. Metal (§0.9): confirm the Pi
               EMMC2 unlock + `/System` autoload + login still work through the
               shared handles.
             - **D2a-2 — persistent `/System` mount + `DriverStoreService` —
               DONE (host-proven + whole gate); metal re-verify pending (§0.9).**
               Keep the `/System` `SharedBlock` alive for life so its read-only
               handle outlives the unlock, without re-architecting the
               metal-proven device bring-up. The unlock kthread becomes a
               **never-returning kernel service** (the sanctioned
               `KernelServiceBody` pattern): because `finish_unlock` takes `blk`
               by value while the device backing (virtio bus / `MmioMap` /
               `DmaPool` / `IrqParkWaiter` / `KernelVirtioHost`, or the EMMC2
               `MmioMap`) stays on the still-suspended `virtio_blk_unlock` /
               `emmc2_unlock` frame, making `finish_unlock` never return keeps
               that whole call chain suspended on the kthread's coroutine stack
               — so the backing stays live with **zero `'static` promotion and
               zero new API surface**, and the proven IRQ-wait/cooperative-yield
               device-driving model is unchanged (best §2.17). A minimal
               `DriverStoreService` owns `shared` + the persistent read-only
               `/System` `SharedBlockHandle`; the existing in-kernel autoload is
               its present, real consumer (no §2.3 speculative surface); the
               service then logs the unlock outcome, releases console 0, and
               **parks** (`CooperativeYield::park`, added delegating to
               `YieldHandle::park` — a real park, never a §2.1 busy-yield),
               holding the mount for life. `finish_unlock` / `virtio_blk_unlock`
               / `emmc2_unlock` / `run_unlock` return
               `Result<Infallible, &'static str>`: success diverges into the
               park, early bring-up errors still return `Err` to the kthread
               closure. (Literal `'static` promotion is rejected: `VirtioBlk` /
               `KernelVirtioHost` *borrow* their backing and the I/O is bound to
               the kthread's IRQ waiter, so promoting and driving from a syscall
               caller's context would re-architect the metal path for no gain —
               D2b's `driver_store_load` instead wakes *this* kthread and reuses
               the proven I/O path, then re-parks.) Metal: re-verify the Pi
               unlock + login (§0.9).
           - **D2b — the user-space migration, over a `/System` file-read IPC
             service (operator-approved this session — supersedes the bespoke
             "driver-store request channel").** The mess of a one-off channel
             bolted onto the parked unlock kthread is replaced by the *general*
             primitive TAIRiX needs anyway: the disk-owning never-returning
             kthread (D2a-2) serves a **read-only `/System` file-read service**
             over the existing capability-gated `IPC_SEND`/`IPC_RECV` ports —
             `open`/`read`/`list`, fail-closed (§5.4), reusing its one proven
             on-disk I/O path. This keeps D2a-2's "the kthread owns and drives
             the disk on its IRQ-bound path" intact while giving it a real
             protocol any client (devmgr now; shell/appmgr later) speaks. Scope
             for this arc: **read-only, `/System`-scoped only** — not a general
             VFS/write/mount surface (that would be §2.3 speculative beyond
             devmgr's need); the server stays **kernel-resident** because the
             floor block driver is in-kernel (§18.6) and the device backing is
             bound to that kthread (a fully user-space fs server is a later,
             larger pivot). `devmgr` (user space) lists `/System/Drivers/`,
             reads each manifest's bind table over the service, matches with
             `lib/devmatch`, and calls `driver_store_load(path, node_id)`
             (`CAP_DRV_LOAD`); the kernel re-reads the bundle over the *same*
             service, re-runs the full signed §8 gate, and spawns with only that
             node's grants — signature/bytes/spawn stay kernel-side (§5.4 /
             §23.1). `hw_tree_read` (`CAP_SYSINFO_HW`) + `hw_tree_wait` (parks on
             the store generation counter) expose the tree. Decomposition (each
             chunk gate-green, §2.3 — a surface lands only with its consumer):
             - **D2b-1 — kernel-resident read-only `/System` file service API,
               consumed by the *existing* in-kernel autoload.** Consolidate the
               store path discovery (`enumerate_driver_store`) and bundle-byte
               reads (`DriverImageReader`/`VfsImageSource`) behind one
               `SystemFileService` that owns the `/System` mount window; the
               current in-kernel `autoload_from_mounted_root` consumes it. Zero
               behaviour change (proven by the autoload host tests + the
               `-M virt` `autoload_input_qemu_aarch64` vertical), no new syscall,
               no metal device-path change — the foundation the IPC endpoint
               wraps in D2b-2.
             - **D2b-2a — the synchronous call/reply IPC primitive (DONE).**
               The store file-read service is request/reply, but `kernel/ipc`
               `Port`s are fire-and-forget and non-blocking. Operator chose a
               *first-class* synchronous primitive over a convention bolted onto
               two async ones. Landed `kernel/ipc::call::CallEndpoint`: a
               capability-gated endpoint correlating each `post`ed request with
               one `reply` via an opaque unforgeable `CallTicket`
               (`recv_call`/`reply`/`take_reply`), bounds grouped in
               `CallEndpointLimits`, fail-closed audit (IDs 3040–3049), a
               poster-only reply claim (`take_reply(claimant, …)`, §19.1), and
               `destroy` cancelling in-flight tickets (§2.9). It is the state
               machine only — never blocks; the caller/server parking is layered
               above through the existing cooperative yield/park seam in D2b-2b
               (§2.2 / §17.4). Host-tested (≥95% tier); no syscall/ABI yet, no
               metal device-path change.
             - **D2b-2b-B — reactive-observe foundation (DONE).** The
               read-only side of the device manager, landed green without the
               production launch. Adds the `hw_tree_read` (no. 29, gated
               `CAP_SYSINFO_HW`) and `hw_tree_wait` (no. 30) `abi-v1` syscalls
               (regenerated C header + `tairix_sys_*` stubs in `lib/abi-sys`), a
               monotonic **generation counter** on `hwtree_store::HW_TREE`
               (bumped on seed/append) served through `HW_TREE_SOURCE` and
               threaded into the dispatch hook by `BootInfo::with_hw_tree` (all
               three Tier-1 boot paths install it), and the signed `devmgr` rxe
               binary whose reactive observe loop (`read → wait → re-read`)
               lives host-tested behind the `tairix_devmgr::HwTreeService` seam
               (`tairix_devmgr::run`). `lib/rt` gains the `hw_tree_read`/
               `hw_tree_wait` wrappers. Host-tested end to end (kernel handlers,
               store + generation, rt wrappers, abi-sys stubs, the seam loop's
               read-and-react-to-one-bump behaviour).
             - **D2b-2b-A — production preemption + true blocking park.**
               A device manager waits **unbounded**; today every kernel wait
               (`irq_wait`, `KernelProcessWait`, `BlockingConsoleRead`,
               `hw_tree_wait`) is a cooperative poll-and-`yield_current`, so a
               perpetual `devmgr` consumes scheduler turns and starves a
               single-CPU system (proven: spawning it timed out the
               `spawn_session`/`root_unlock`/`autoload_input` `-M virt`
               verticals — §2.1). The correct fix is a task that truly **parks**
               off the run queue, which in turn needs the kernel to be
               **preemptive** so a parked-everyone CPU still wakes a timed waiter
               and a CPU-bound task cannot monopolise a core (operator decision:
               *all* architectures preemptive — §4 SMP, §2.16 performance).
               There is no IRQ-driven preemptive context switch yet: the kthread
               model only suspends cooperatively at syscall traps
               (`reschedule_current`), EEVDF is tickless (`on_timer_tick` is a
               counter), and aarch64/riscv64 production never arm the timer.
               Staged P-1..P-3, one fully-gated landing each (operator-approved):
               - **P-1 — production timer-IRQ-driven preemption (all arches).**
                 Arm the periodic generic-timer interrupt in the production boot
                 on aarch64 + riscv64 (x86_64 already arms it, with a null
                 callback) and build the IRQ-context preemptive reschedule behind
                 the Arch HAL (§17.2): a timer IRQ **taken from EL0** lands on the
                 interrupted task's own kernel stack (the same stack a syscall
                 trap uses), so after the GIC/EOI handshake it suspends that task
                 back to the scheduler via the existing `reschedule_current` —
                 the involuntary analogue of the cooperative `Yielder::suspend`,
                 sharing one context-switch definition (§2.2); the trampoline
                 already saves/restores `ELR_EL1`/`SPSR_EL1`/`SP_EL0` per
                 exception so the resume is correct. A tick taken in **EL1** never
                 preempts — the kernel is non-preemptible, so a held lock or
                 in-flight syscall is never abandoned (§4 SMP watch-out).
                 **Prerequisite (aarch64):** EL0 currently erets with
                 `SPSR.DAIF = 0b1111` (all masked — `userentry`), so no IRQ fires
                 in user mode; preemption requires unmasking IRQ (`I`) in the EL0
                 entry `SPSR`, after which **all** enabled interrupts (timer +
                 device, e.g. virtio) are taken in user mode, not only while a
                 kthread is parked in EL1 — a metal-affecting change verified on
                 the Pi (§0.9). Landed per-arch (P-1a aarch64 first, the metal
                 target; P-1b riscv64; P-1c x86_64). Proven on `-M virt`
                 verticals (a CPU-bound EL0 task is involuntarily preempted) +
                 host tests; SMP-correct (§4), fail-closed/fail-safe (§5.4/§2.9);
                 Pi metal checklist (§0.9).
                 **P-1a (aarch64) DONE — whole gate green and metal-confirmed
                 on a real Pi 4B.** `userentry` erets to EL0 with IRQ unmasked
                 (`SPSR_EL0T_PREEMPTIBLE`); a fail-safe EL0-only preempt-callback
                 hook in `tairix_arch_aarch64::preempt` (`set_preempt_callback` /
                 `on_el0_preempt_point`) is invoked from
                 `exceptions::handle_irq(from_el0)` after EOI only for an
                 EL0-origin timer tick (EL1 ticks never preempt); and
                 `gic_irq::arm_preemption()` (called from
                 `Aarch64BinArch::install_irq_dispatch`) registers
                 `PreemptStorage<1>`, installs the `reschedule_current(Yield)`
                 callback, and sets up the tickless one-shot generic timer
                 (armed per-dispatch by the scheduler — P-4). No tick callback
                 (EEVDF is tickless; armed solely to preempt). The behavioural
                 proof is the `preempt_el0_qemu_aarch64` `-M virt` vertical: a
                 runaway, never-yielding EL0 spinner (the pure-Rust
                 `el0_spinner` fixture, a `black_box` busy loop issuing no
                 syscall) is involuntarily preempted at least once **and**
                 correctly resumed mid-loop to run to completion and exit — the
                 only way it leaves EL0 before `exit` is a forced preemption.
                 **P-1b (riscv64) DONE — whole gate green (Pi metal N/A:
                 riscv64 is QEMU `virt`/SiFive).** A U-mode-only preempt-callback
                 hook in `tairix_arch_riscv64::preempt` (`set_preempt_callback` /
                 `on_u_mode_preempt_point`) is invoked from the supervisor-timer
                 branch of `trap::tairix_riscv64_trap_handler` after the SBI
                 re-arm, gated on the saved `frame.sstatus` SPP == 0
                 (`trap::trap_came_from_user`); `tairix_kernel::riscv64::boot`'s
                 `arm_preemption()` (called from a new
                 `RiscvBinArch::install_irq_dispatch`) registers
                 `PreemptStorage<1>`, installs the `reschedule_current(Yield)`
                 callback, derives the interval from the device-tree
                 `timebase_hz`, and `init_local_preempt`s. Crucially riscv64
                 needs **no** EL0-SPSR-style change and **no** global-`SIE`
                 enable: a supervisor timer interrupt is taken in U-mode by the
                 privilege rule (U < S) regardless of `sstatus.SIE`, so leaving
                 `SIE` clear keeps the kernel non-preemptible while U-mode is
                 preemptible. The behavioural proof is the
                 `preempt_el0_qemu_riscv64` `-M virt` vertical (reusing the shared
                 `el0_spinner` fixture, armed via `install_trap_vector`), PASSing
                 (`EventId(4330)`) once the runaway U-mode task is involuntarily
                 preempted ≥ 1 and resumed to exit.
                 **P-1c (x86_64) DONE — whole gate green.** A ring-3-only
                 preempt-callback hook in `tairix_arch_x86_64::preempt`
                 (`set_preempt_callback` / `preempt_callback`, plus the pure
                 `cs_is_ring3` origin test) is invoked from the LAPIC-timer ISR
                 `tairix_arch_x86_64_timer_dispatch` — **after** the LAPIC EOI
                 (so the in-service bit is released before the context switch) and
                 **only** for a tick whose saved interrupt-frame `CS` has RPL 3 —
                 bracketed by the `swapgs` pair that establishes the in-handler GS
                 convention the kthread cooperative-park balance expects and
                 restores the user GS before `iretq` (symmetric with the `syscall`
                 stub). `userentry` now `iretq`s to ring 3 with `RFLAGS.IF` set
                 (preemptible user mode); the kernel issues no `sti`, so it stays
                 non-preemptible (IF == 0) and a maskable tick is taken only in
                 ring 3. The bin's `BinArch::install_irq_dispatch` installs the
                 `reschedule_current(Yield)` callback (the LAPIC timer is
                 programmed one-shot and disarmed at boot step 8, then armed
                 per-dispatch by the scheduler — P-4). Because x86_64 delivers a ring-3
                 interrupt through the IDT gate using `TSS.RSP0` (distinct from the
                 `syscall` `gs:0` stack), the per-resume `syscall_entry::
                 set_kernel_rsp0` now repoints **both** the `gs:0` and `TSS.RSP0`
                 entry stacks at the task's own kernel stack — one definition
                 (§2.2) so a preemption (or fault) can never land on another
                 task's stack. The behavioural proof is the
                 `preempt_el0_qemu_x86_64` vertical (boots the production pipeline;
                 maps the supervisor-only LAPIC page; single `el0_spinner`),
                 PASSing (`EventId(4337)`) once the runaway ring-3 task is
                 involuntarily preempted ≥ 1 and resumed to exit.
                 **Next: P-2.** P-1 (production timer-IRQ-driven preemption) is now
                 complete on all three bare-metal targets.
               - **P-2 — generic blocking wait-queue + true `Park` + timed wake.**
                 A reusable kernel wait primitive: a task registers on a wait
                 object and parks (`RescheduleAction::Park`, off the run queue),
                 woken by an explicit event or, with a deadline, when the timer
                 fires for the nearest expiry. A timed wait's deadline is one of
                 the events that arm the scheduler's one-shot timer (P-4), so the
                 timed wake is itself tickless — it MUST NOT depend on a
                 fixed-frequency periodic sweep (`AGENTS.md` §17.1).
                 `hw_tree_wait` is its first consumer —
                 `HwTreeStore::seed`/`append` (and later node removal) wake every
                 waiter; the finite `timeout_ns` is now honoured by the timed
                 wake. No busy-yield, no lost wake-ups.
               - **P-3 — production launch + devmgr vertical.** `init` spawns the
                 perpetual `/System/Services/devmgr` (no longer starving the CPU,
                 since the wait truly parks), and a new end-to-end devmgr QEMU
                 vertical proves spawn → read → react to a real generation bump.
               - **P-4 — tickless (NO_HZ) preemption: one-shot, scheduler-armed
                 — DONE (host + all three `-M virt`/QEMU preempt verticals +
                 SMP stress; metal-confirmed on a real Pi 4B, §0.9).** The
                 preemption timer is armed **one-shot** to one scheduling quantum
                 only while a CPU is contended, and disarmed when a CPU runs a sole
                 runnable task — no fixed-frequency periodic arming remains
                 anywhere (`AGENTS.md` §17.1 NO_HZ). Shape as built:
                 - The Arch HAL timer surface (`tairix_arch_api::timer::Timer`)
                   gained `arm_oneshot(ticks_from_now)` / `disarm()` over each
                   port's `CNTP_TVAL_EL0` / SBI `set_timer` (disarm = far-future) /
                   LAPIC one-shot initial-count (disarm = count 0). Each port's
                   `init_local_preempt` records the per-quantum interval but leaves
                   the timer disarmed, and `on_timer_interrupt` no longer re-arms.
                 - The scheduler decides on each dispatch via the provided
                   `SchedulerArch::set_preemption(armed)` (default no-op), where
                   `armed` = "this CPU still has a ready competitor" (the per-CPU
                   ready run-queue length, since the running task sits in the
                   current slot). The port arms the stored quantum or disarms;
                   `on_timer_tick` stays observation-only. The shared quantum rate
                   is `tairix_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ` (one
                   definition for aarch64+riscv64; x86_64 keeps its 1 ms LAPIC
                   calibration period).
                 - MLFQ (§17.1 carve-out): there is one per-CPU timer and the boost
                   interval ≫ one quantum, so the anti-starvation boost rides the
                   on-demand preemption one-shots that fire only while a CPU is
                   contended (exactly when starvation is possible) — no global
                   fixed-frequency tick is reintroduced. Documented in the MLFQ
                   crate rustdoc + `docs/src/architecture/scheduler.md`.
                 - Verticals: the `preempt_el0_qemu_{aarch64,riscv64,x86_64}`
                   verticals now spawn a competitor alongside the runaway spinner
                   (so the one-shot arms and a preemption fires) and assert the
                   **sole** spinner that runs after the competitor exits takes **no**
                   further timer interrupt (the disarm). `scheduler_stress_qemu`
                   uses busy self-terminating witnesses per CPU to drive each CPU's
                   one-shot to its per-CPU preemption threshold under contention.
               - **P-5 — fully-preemptive kernel: no cooperative dispatch loop
                 — DONE (host + all QEMU verticals green, incl. the dedicated
                 in-kernel vertical; metal re-confirmation pending).** The bare-metal
                 dispatch loop no longer runs in-kernel tasks/kthreads with device
                 interrupts masked (the cooperative model §17.1 forbids, and the
                 structural cause of the serial-stall saga). Shape as built:
                 - **Loop runs with device IRQs enabled.** `kernel/core`'s
                   `admit_init` dispatch loop calls the new
                   `KernelArch::set_device_irqs(true)` once before steady-state
                   dispatching, so every in-kernel task/kthread it runs executes
                   with interrupts deliverable — a long in-kernel operation (the
                   `pcie_brcm` MISC read-back, a busy driver poll) can no longer
                   mask interrupts for its whole span and starve the preemption
                   one-shot, the buffered-serial TX drain (§20), or an
                   interrupt-driven waiter. The loop masks only around the idle
                   park (race-free: mask → drain → `wfi`/`hlt` → unmask) and
                   before halt. Each port backs `set_device_irqs` with its PE-level
                   primitive (`DAIF.I` / `sstatus.SIE` / `RFLAGS.IF`); the idle
                   `wait_for_interrupt` is a pure `wfi` (aarch64/riscv64) or the
                   atomic `sti;hlt;cli` (x86_64, since `hlt` needs `IF=1`).
                 - **Non-preemptible kernel preserved (§4).** A device IRQ taken
                   while an in-kernel task runs services its source and returns to
                   the *same* task; only a timer tick taken from EL0/U-mode/ring 3
                   reschedules — every port already gates preemption on the
                   interrupted privilege (`from_el0` / saved `SPP` / `cs_is_ring3`),
                   so enabling interrupts in EL1/S-mode/ring 0 never preempts the
                   kernel itself.
                 - **Lock-discipline: ISR handlers are lock-free; the unpark is
                   deferred (operator-approved design).** Rather than make the
                   scheduler's locks IRQ-safe (an IRQ-safe `RwLock` + IRQ-off on
                   the hottest scheduler path, §2.16), the interrupt-reachable
                   wakes (`console_wake`, `irq_wake`, `timed_wake_sweep`) are now
                   **lock-free**: each only sets an atomic flag
                   (`WaitQueue::request_wake` / `TIMED_SWEEP_PENDING`, mirroring
                   `IrqTable::fire`). The real `WaitQueue::wake_all` / deadline
                   sweep + scheduler `unpark` runs at a safe dispatcher-context
                   point (`waitq::drain_pending_wakes`, called between steps and
                   before idle), where taking the scheduler/run-queue locks cannot
                   deadlock against an interrupted task. A woken task cannot run
                   until the current in-kernel task yields anyway (§4), so
                   deferring the unpark costs no responsiveness. No scheduler lock
                   is ever taken with interrupts disabled.
                 - **Validation:** `kernel/core` host tests (incl. new
                   `waitq::request_wake*` cases), all three bare-metal targets
                   build, and the full `cargo xtask test --qemu` matrix passes
                   (devmgr park/wake, `wait`, `irq`, `uart-console`,
                   `root-unlock-login`, `preempt_el0` on every arch, and the
                   in-kernel `preempt_inkernel_qemu_aarch64` vertical).
                 - **Residual cooperative reader removed (the metal stall cause).**
                   P-5 made the *dispatch loop* preemptive, but the interactive
                   root-unlock kthread still read the passphrase through a
                   **cooperative busy-poll** (`KthreadConsoleRead` looped
                   `yield_now` over an RX-masked raw-FIFO poll), so that kthread
                   was always runnable, the loop never reached idle, `pump_tx`
                   never ran, and on metal (where the PL011 TX "FIFO-has-room"
                   interrupt does not self-sustain the drain) console output only
                   advanced when a keystroke's echo incidentally flushed the FIFO
                   — "type → progress → stall". Fixed: the unlock kthread now
                   reads the **interrupt-fed** console queue (`UART_CONSOLE_READ`
                   over `UART_INPUT`, or the video keyboard queue) and **parks**
                   off the run queue between empty polls — it registers its
                   scheduler id (`set_unlock_console_task`/`unlock_console_task`)
                   on `CONSOLE_WAITQ` (register-before-poll lost-wakeup interlock)
                   and the console RX interrupt's `console_wake` unparks it. The
                   UART branch enables RX for the unlock window
                   (`enable_uart_console_irq`, now idempotent with the `login`
                   handoff); the video branch needs no RX enable (keyboard
                   injection wakes it). The loop now idles while waiting for the
                   passphrase, so `pump_tx` and tickless idle run and the console
                   drains. Host-proven (`KthreadConsoleRead` parks, never
                   busy-yields).
                 - **Buffered serial drain decoupled from idle (the second metal
                   stall cause).** Even with the unlock reader parking, the debug
                   log still froze the instant `Root passphrase:` appeared: the
                   USB-keyboard **report-pump kthread** (`keyboard_service::
                   spawn_pump`) runs `loop { pump_once(); yield_now(); }` — it is
                   *perpetually runnable*, so the dispatch loop never reaches its
                   `Idle` arm, where `pump_tx` (the buffered-UART top-up) was the
                   only reliable drain on metal (the PL011 TX "has-room" interrupt
                   does not self-sustain the drain). Fixed structurally, not by
                   touching the keyboard pump: the buffered console transmit now
                   drains on **every** dispatch-loop iteration through a new
                   default-no-op `KernelArch::pump_console_tx()` seam — called in
                   the Ran arm (`KernelInitSpawner::service_between_dispatches`,
                   alongside `drain_pending_wakes`) *and* before the idle `wfi`.
                   aarch64 overrides it with the non-blocking `serial::pump_tx`
                   (moved out of `wait_for_interrupt`, now a pure park); riscv64/
                   x86_64 inherit the no-op (synchronous console output). Output
                   now flows at the loop's dispatch rate regardless of idle and
                   independent of the transmit interrupt. Host-proven
                   (`service_between_dispatches_tops_up_the_console_transmit`:
                   the count was `0` before the fix, increments per dispatch now).
                 - **PL011 transmit interrupt fires when the FIFO runs dry (the
                   third metal stall cause).** The transmit interrupt was only
                   *unmasked* (`TXIM`); its FIFO trigger level was never
                   programmed, so it sat at the PL011 reset default of 1/2 full.
                   On the Pi 4's flow-blocked UART a small ring drain never lifts
                   the transmit FIFO above 1/2, so it never transitions back down
                   through the trigger and the level-based transmit interrupt
                   never re-asserts — the background drain (`service_uart_tx_irq`)
                   stalls until something else pushes bytes. **Fixed:**
                   `ConsoleModel::tx_interrupt_enable` now first lowers the
                   transmit trigger to its lowest level (`UARTIFLS.TXIFLSEL` →
                   1/8 full) and then unmasks `TXIM`, so the interrupt fires the
                   moment the hardware FIFO runs dry and reliably re-arms the
                   refill ISR; `serial::enable_tx_interrupt` applies the 2-step
                   sequence (mirroring the RX `IFLS`-then-`IMSC` shape, §2.2). The
                   mini-UART needs no trigger step (its TX interrupt already fires
                   on "holding register empty"). Host-proven
                   (`tx_interrupt_enable_lowers_the_trigger_then_unmasks_only_tx`).
                 - **EMMC2 SD reads are interrupt-driven, not busy-spun (the
                   structural §17.1 cause of the residual ~15 s stall).** The SD
                   driver's completion waits (`wait_interrupt`) busy-spun the
                   SDHCI status register with `core::hint::spin_loop()` up to the
                   poll budget, never yielding. Because the kernel is
                   non-preemptible for in-kernel kthreads, every SD read froze the
                   dispatch loop for the whole disk-I/O span, so `pump_tx` could
                   not run — and around the passphrase prompt the driver-store
                   serve loop hammers `/System` to satisfy `devmgr`'s autoloads,
                   so that burst of busy-spun reads was the ~15 s the log stalled
                   for. **Fixed (operator-directed, the more-correct option):**
                   `reset_and_clock` enables the controller's completion-signal
                   sources (`IRPT_EN`), and `wait_interrupt` now **parks** on the
                   controller's interrupt through a `SdhciHost::await_irq` /
                   `CompletionWait` seam between status re-reads — never a
                   busy-spin (§17.1/§2.16). The kernel supplies that seam:
                   `emmc2_unlock` discovers the EMMC2 GIC SPI (`emmc2_spi`),
                   binds/routes/arms it on the published IRQ table, and blocks on
                   it through the same task-parking waiter the virtio path uses
                   (`Emmc2Completion` over `tairix_kernel_core::IrqParkWaiter`,
                   §2.2; the boot kthread takes the bounded race-free `wfi`
                   fallback, and a controller silent past the 2 s budget fails
                   closed as `DeviceFault`).
                   The identification-only handshakes that have no completion
                   source (reset, clock-stable) still spin, each poll-budget
                   bounded and fail-closed (§2.1). Host-proven
                   (`interrupt_driven_read_parks_until_the_controller_signals`,
                   `reset_enables_the_completion_interrupt_signal`); metal
                   re-confirmation pending (QEMU models no Pi EMMC2, §0.4).
                 - **In-kernel preemption vertical — DONE (QEMU-proven).** The
                   dedicated QEMU vertical that proves the in-kernel case directly
                   is `tests/integration/preempt_inkernel_qemu_aarch64` (enrolled
                   in the `cargo xtask ci` matrix). It boots the `virt` board,
                   installs the production `tairix_arch_aarch64::preempt` surface
                   verbatim (§2.2 — `PreemptStorage`, the EL0-preempt callback, a
                   timer-tick callback, the enabled generic-timer PPI), builds a
                   live eevdf `Scheduler`, and admits ONE in-kernel kthread that
                   arms the timer one-shot and then busy-loops issuing no `yield`
                   and no syscall, with device IRQs enabled at the PE
                   (`exceptions::enable_irq` — the aarch64 backing of
                   `KernelArch::set_device_irqs(true)`). It PASSes once a timer IRQ
                   was taken *during* the busy span (the EL1 tick callback fired),
                   the EL0-preempt callback fired **zero** times (the kernel itself
                   was never preempted, §4), and the kthread resumed and ran to its
                   voluntary completion. Under the old cooperative loop (device
                   IRQs masked across the whole task run) no tick would be taken and
                   the kthread would spin forever, so the test fails loud (a
                   finisher code or the harness timeout, §7). The debug-only
                   `pcie_brcm` MISC read-backs the metal stall blamed are already
                   deleted (metal bring-up confirmed, §2.14).
                 - **Post-preemption IRQ restoration — DONE.** A timer exception
                   masks interrupt delivery before suspending an EL0 task. The
                   CPU mask is not task context, so switching directly back to
                   the dispatcher used to leave that CPU permanently masked;
                   under four-core user load every CPU could eventually lose
                   timer/device progress and freeze service startup. The shared
                   dispatch loop now restores device IRQ delivery immediately
                   after every returned scheduler step, at the lock-free safe
                   boundary before deferred wakes or another dispatch. A host
                   regression models the masked return deterministically, and
                   the four-core stress vertical is the production coverage.
                 - **Remaining (metal only):** metal re-confirmation on a real Pi 4
                   that boot stays responsive through the slow PCIe read-back *and*
                   that the debug log keeps flowing past the passphrase prompt
                   (§0.9). The polled USB-keyboard pump remains CPU-hungry (it
                   yields but never parks); making xHCI HID interrupt-driven so it
                   parks is a separate §2.16 efficiency follow-up, not a correctness
                   blocker now that the serial drain no longer depends on the loop
                   idling.
                 - **Console-render burst cost bounded; `ls` streams (the
                   `ls -lsR /` UART-stutter fix).** Two defects made a large
                   listing on the Pi 4 video console dump nothing for ~20 s and
                   then stall the UART debug log for seconds at a time: (1)
                   `ls` rendered the whole recursive listing into one buffer
                   and wrote it once at the end — it now writes each directory
                   block as it is read (`userland/apps/ls`, memory bounded by
                   the largest single directory, never the tree); (2) the
                   `lib/fbcon` engine scrolled the *pixels* once per line
                   feed (a near-whole-framebuffer `copy_within` per line), so
                   one 4 KiB `console_write` performed dozens of framebuffer
                   copies inside a single non-preemptible syscall span,
                   starving every other task and the serial producers. The
                   engine is now grid-first: every `Op` mutates only the
                   retained cell grid and the dirtied cell rect is repainted
                   **once** per write (blank runs span-filled; the framebuffer
                   is written, never read), so a burst's render cost is one
                   bounded repaint. Metal re-confirmation of smooth concurrent
                   HDMI + UART output rides the same §0.9 pass as above.
                 - **Pending-preemption latch: a quantum expiring mid-syscall
                   is honoured at the syscall boundary, never lost.** The
                   ports preempt only a timer tick taken from user mode (the
                   kernel is non-preemptible, §4); a tick taken *in* a syscall
                   used to disarm the one-shot, clear the quantum, and do
                   nothing more — the task then resumed user mode with no
                   timer armed and ran unpreempted until its next voluntary
                   yield, so a syscall-heavy task (`ls -lsR /`) could starve
                   every competitor (cooperative in practice). Now
                   `kernel/core::preempt` keeps one lock-free per-CPU latch:
                   every port's per-tick callback latches the fired tick, the
                   syscall dispatch hook converts a completed syscall into the
                   `yield` suspension when the latch is set
                   (`completion_outcome` → `Reschedule { Yield }`), and
                   `dispatch_step` clears the latch before switching a task in
                   (a fresh dispatch decision supersedes it, so a user-mode
                   tick never doubles into a spurious yield). Quantum overrun
                   is bounded by the remainder of one syscall (each syscall's
                   in-kernel work is itself bounded, e.g. `console_write`'s
                   4 KiB clamp). Design: `docs/src/architecture/scheduler.md`.
                 - **CPU-lockup watchdog — first-class soft + hard detector
                   (host-proven; aarch64 metal validation pending). Design:
                   `plans/WATCHDOG.md`.** Detects, diagnoses, and best-effort
                   recovers from two failures, loud enough to explain *why*.
                   `kernel/core::watchdog` keeps two per-CPU heartbeats plus an
                   activity class (Offline/Idle/Active) so only a CPU that owes
                   progress is judged (fail closed).
                   **Soft lockup** (a CPU that keeps taking interrupts but
                   stops returning to the scheduler): a progress heartbeat
                   stamped per dispatch-loop iteration (`note_progress`),
                   sampled by the armed preemption tick (`check_stall`,
                   contended CPUs only — no lone-task false positive) and the
                   cross-CPU scan; a gap past
                   `DEFAULT_SOFT_LOCKUP_THRESHOLD_NS` (10 s) emits
                   `CPU_STALL_DETECTED` (4080) / `_CLEARED` (4081).
                   **Hard lockup** (a CPU that stops taking even interrupts):
                   only another CPU can see it. A port arms a non-maskable
                   ~1 Hz cadence sample (Arch HAL `tairix_arch_api::watchdog`,
                   `WatchdogArch`/`WatchdogSample`) that stamps a liveness
                   heartbeat and runs a cross-CPU scan (`on_watchdog_tick`); a
                   buddy stale past `DEFAULT_HARD_LOCKUP_THRESHOLD_NS` (10 s)
                   emits `CPU_HARD_LOCKUP_DETECTED` (4082) / `_CLEARED` (4083).
                   Each sample records the interrupted PC/PSTATE/kernel-vs-user
                   as last-known context, so a detection carries `cpu`,
                   `observer`, `stalled_ms`, `pc`, `pstate`, `context`. A hard
                   lockup's recorded sample is stale (taken before the CPU went
                   silent), so it is marked `sampled=pre_silence` and the
                   observer names the live stuck controller line as
                   `stuck_irq=<id>` plus `stuck_state=<active|pending>` via the
                   Arch-HAL query `WatchdogArch::stuck_interrupt` returning a
                   `StuckInterrupt {intid, active}` (default `None`; aarch64 reads
                   the lowest *deliverable* GICv2 SPI through `gic::stuck_spi` —
                   active unconditionally, pending only when its `GICD_ISENABLER`
                   bit is set — host-tested against the mock distributor). Only a
                   line that can still reach a CPU is ever reported: a masked line
                   cannot be the wedge, so it is skipped rather than blamed (the
                   fix for the spurious `stuck_irq=111` — a masked, unowned line
                   the old any-pending fallback reported). `stuck_state`
                   distinguishes a live storm (`active`) from an enabled line
                   asserted but not yet taken (`pending`). The observer also
                   attributes the stuck id against
                   the live kernel IRQ table (`IrqTable::owner_of_line` via the
                   arch-neutral `watchdog::StuckOwnerResolver` seam installed over
                   `&KernelState.irq`), rendering `stuck_owner=<task>` for a line
                   a driver bound or `stuck_owner=unbound` for a spurious /
                   kernel-contained line no driver owns — so a raw `stuck_irq=111`
                   is decidable as "not the USB path" without a device tree.
                   Omitted when no resolver is installed (never a claim it cannot
                   make); host-tested via the pure `resolve_stuck_owner_with` +
                   `owner_of_line`. A detection drives
                   a best-effort `WatchdogArch::request_recovery` (reschedule
                   for soft, directed attention for hard), recorded with its
                   honest outcome as `CPU_LOCKUP_RECOVERY` (4084). All paths
                   are lock-free/allocation-free and fail closed before the
                   hooks are installed. aarch64 delivery: the virtual generic
                   timer (`CNTV`, PPI 27) as an IRQ (the correct and complete
                   buddy detector for a GICv2 non-secure kernel, where FIQ is
                   the secure-world channel a non-secure kernel cannot route;
                   `plans/WATCHDOG.md`). x86_64/riscv64
                   keep the soft detector and inherit hard detection when they
                   wire their own cadence + `WatchdogArch`. Design:
                   `docs/src/architecture/scheduler.md` +
                   `docs/src/architecture/kernel.md` audit catalogue.
                 - **Runaway-interrupt quarantine — the storm root-cause fix
                   (host-proven).** The `usb_mouse`/`xhci` "100% CPU" +
                   hard-lockup symptom is a bound line (a wedged/never-quiesced
                   controller, or a hostile device) that re-asserts every time
                   the kernel re-arms it, pegging a CPU through the
                   mask/wake/re-arm cycle; the "100% task" is just ISR-time
                   attribution to whichever task sits on that core.
                   `kernel/irq::IrqTable` now rate-limits each line: `fire`
                   counts fires over a 1 s window (against a boot-installed
                   lock-free `MonotonicClock` seam, `SchedWaitQueueArch`) and,
                   past `STORM_FIRE_BUDGET` (100 000/window), **quarantines**
                   the line — keeps it masked, stops delivering `ready`, and
                   the parked `irq_wait`/`waitset_wait` waiter fails closed with
                   `Errno::DeviceFault` (`WaitStep`/`WaitOutcome::Quarantined`).
                   The disable is audited once at the syscall boundary as
                   `IRQ_LINE_QUARANTINED` (4090, `line`+`task`); a fresh
                   `irq_bind` clears it. Linux `note_interrupt` analogue for the
                   user-space IRQ model; generous budget so a busy-but-healthy
                   line never trips; inert until the clock installs (fail-open
                   on the net, never on security). Design:
                   `docs/src/security/irq.md` (Runaway-line quarantine) +
                   `docs/src/architecture/kernel.md` (4090). The separate
                   Pi-only question of *why* the VL805/xHCI re-asserts is now
                   chased from this audit trail rather than a wedged core.
                 - **Verbose kernel-panic post-mortem — registers + a bounded
                   backtrace on every image (host-proven; QEMU e2e per arch).
                   Design: `plans/FIX-PANICS.md`,
                   `docs/src/architecture/panic-diagnostics.md`.** A panic now
                   emits a register snapshot and a frame-pointer backtrace, not
                   one line. A new closed Arch HAL slice
                   `tairix_arch_api::backtrace` (`CpuStateCapture`:
                   `capture` + a pure `FrameLayout` + `stack_bounds` + honest
                   `BacktraceProfile`, plus its `conformance` vertical) is
                   implemented honestly on x86_64/aarch64/riscv64 and an honest
                   `Unsupported` on wasm32. The single bounds-checked, monotonic,
                   depth-capped (64) frame-pointer `walk` lives once in
                   `kernel/arch/api` and reads only through a `StackReader`, so
                   the one dangerous dereference site is shared and fuzzed
                   (`fuzz_backtrace`), never copied per arch. `kernel_core::panic_dump`
                   gained a per-boot re-entrancy guard, `format_hex_u64`, and the
                   register/`frame_N` audit fields (kernel addresses are printed
                   deliberately — fatal/halting — while user-fault kills still
                   omit the user address). The three divergent *production* panic
                   bridges are collapsed onto this one path via the bin-crate
                   `panic_ctx` publish-arch-ptr pattern (x86_64/aarch64/riscv64);
                   the arch-crate `handle_panic_via_serial` stays as the QEMU
                   test-harness park-on-panic helper. **Staged follow-up:**
                   on-target symbolication (a `cfg!(debug_assertions)`-gated
                   compiled-in `(addr, name)` table); the zero-image-cost default
                   everywhere is raw addresses resolved offline with `addr2line`.
               - **P-5b — syscalls run with interrupts enabled (the
                 syscall-entry half of the §17.1 no-cooperative-dispatch fix).
                 Design: `plans/FIX-SYSCALL.md`.** P-5 made the *dispatch loop*
                 and in-kernel kthreads preemptible, but the user→kernel syscall
                 path still ran the whole syscall with device IRQs and the
                 preemption timer masked, so a long non-blocking body (a
                 bootstrap-floor `fs_*` MMIO wait) monopolised the CPU exactly as
                 the pre-P-5 loop did. Now every bare-metal port's trap glue
                 unmasks device IRQs for the syscall *body* only and re-masks on
                 exit (aarch64 `DAIF.I` around `dispatch_svc`, riscv64
                 `sstatus.SIE` around `dispatch_ecall`, x86_64 `sti`/`cli` around
                 the `syscall_entry_stub` dispatch call; wasm32 is a no-op — no
                 hardware interrupts). The kernel stays non-preemptible (§4): an
                 IRQ taken in EL1/S-mode/ring-0 is a nested trap that services its
                 source and returns to the same syscall, gated by the existing
                 `from_el0`/`SPP`/ring-3-`CS` preempt gates; its reschedule is
                 latched. The one arch-neutral `completion_outcome`
                 (`kernel/core`) drains the deferred, lock-free ISR wakes
                 (`waitq::drain_pending_wakes`) and honours a latched tick /
                 unparked task with a `Yield` at return-to-user — reusing P-5's
                 machinery, no per-syscall flag, no second wake discipline. Safe
                 by lock discipline: every ISR is lock-free (`request_wake`) and
                 the only ISR↔task-shared ring (aarch64 console RX) is
                 `UART_RX_GATE`-interlocked; riscv64/x86_64 console reads are the
                 fail-closed `NULL_CONSOLE_READ` (no RX ISR). The standing rule
                 "any ISR-shared lock is `IrqSafeSpinLock` or the ISR side is
                 lock-free + deferred drain" is in `lib/sync`'s `irq` rustdoc and
                 the §23 checklist. Docs: `docs/src/architecture/syscalls.md`.
               - **P-5c — in-kernel bodies yield at a safe boundary (the
                 in-kernel half of the §17.1 no-cooperative-dispatch fix) —
                 DONE.** P-5 made the dispatch loop preemptible and P-5b the
                 syscall body, but both latches are consumed only on the way
                 back to *user* mode, so an in-kernel body that issues one
                 bounded operation after another still held its CPU for the whole
                 burst. It does not spin and each operation blocks correctly, so
                 a *slow* device parks it and the dispatcher runs — but when the
                 device is fast enough that no operation ever waits
                 (`virtio_blk::submit_and_wait` polls the completion ring
                 *before* waiting, and under QEMU the completion is already
                 there) the park never happens and the whole burst runs with the
                 dispatch loop's housekeeping and heartbeats suspended. A desktop
                 session reading wallpaper JPEGs reproduced it as a 10 s
                 `context=kernel` soft lockup with no spin anywhere
                 (`plans/OPEN-DEFECTS.md` D24). `preempt::yield_if_owed` is the
                 boundary: it consumes the same latch and applies the same
                 competitor-gated decision the return-to-user point applies (one
                 shared `honour_latched_tick`, not a second policy), so a burst
                 gives the CPU up at most one operation after its quantum expires
                 and costs one atomic read when nothing is owed. Placement is the
                 caller's obligation — only where no spin lock is held, which a
                 point that can already park on a slow device satisfies by
                 construction. Call sites: the storage funnel every in-kernel
                 device operation passes through (`SharedBlockHandle::with_device`,
                 before the shared device's sleeping lock) and the in-kernel
                 `/System` store server's between-requests boundary. The
                 dispatcher also stamps a distinct `k_site=kernel_body` crumb for
                 a kernel kthread body, which previously shared `user_switch`
                 with a user task's EL0 run and so pointed a reader at a
                 misbehaving program. Docs:
                 `docs/src/architecture/scheduler.md`,
                 `docs/src/drivers/block.md`, `plans/WATCHDOG.md`.
               - **P-6 — wait-queue §27 completeness rework — DONE
                 (host-proven).** `kernel/core/src/waitq.rs` now meets the §27
                 bar. The P-2 slice's O(n) `Vec` wait set is replaced by a
                 three-index `WaitSet` (all `BTreeMap`, `const`-constructible so
                 the `static` queues keep `const fn new()`): `by_task`
                 (membership, O(log n) `register`/`deregister`/`wake_task`),
                 `order` (arrival `seq` → task, so `wake_one`/`oldest_task` take
                 the FIFO head in O(log n) — a *stated* first-come-first-served
                 no-starvation discipline; a re-`register` keeps its `seq`, so a
                 looping waiter is never overtaken), and `deadlines`
                 (`(deadline, seq)` → task, only finite deadlines, so
                 `earliest_deadline` is O(log n) and `sweep` visits only the
                 expired prefix in deadline order, O(log n + woken), not a scan
                 of every waiter per timer expiry). `wake_all` stays O(n) for
                 genuine broadcast conditions only; `wake_one`/`wake_task` (the
                 targeted `CallEndpoint`-reply / IPC-server / signal-intake
                 wakes) are the single-target path. The lock-free ISR
                 `request_wake` + deferred `drain_pending_wakes` shape P-5 landed
                 is unchanged (§2.2 — one wake/drain discipline). No surface
                 beyond the abstraction itself (§27.4). Every park site was
                 re-audited: single-target events use `wake_task`
                 (`CALL_WAITQ`/`SERVE_WAITQ`/`SIGNAL_INTAKE_WAITQ`), genuine
                 broadcasts use `wake_all` (`CONSOLE`/`PROCWAIT`/`PIPE`/
                 `HW_TREE`/`USERS_DB`/`APP_STORE`/`SEAT_INPUT`). Host tests cover
                 FIFO order + re-register position preservation, deadline
                 ordering + expired-prefix sweep, deregister across every index,
                 the wake-one round-robin no-starvation loop, and the unchanged
                 lock-free `request_wake`/drain race.
               - **P-7 — §27 foundational-primitive audit sweep — DONE.** The
                 general §27 sweep (`plans/OPEN-DEFECTS.md` D4) of every
                 foundational primitive other code builds on: the `lib/sync`
                 locks (`SpinLock`/`IrqSafeSpinLock`, the fair FIFO `McsLock`,
                 the writer-preference `RwLock`, `SeqLock`), `OnceCell`/`Once`,
                 `lib/collections::BitSet256`, `lib/caps` (`CapabilitySet`
                 delegation + `CapToken`), `kernel/ipc` (`PortRegistry` +
                 `call`/`port`/`notify` over the P-6 wake/drain), and the
                 allocators (`lib/kalloc` coalescing free list, `lib/rt` heap
                 free-span, `kernel/mem::Slab`). All are §27-complete abstractions
                 with the right structure/complexity for §26 load; `waitq` (P-6)
                 was the sole thin slice. **One latent structural watch-item,
                 staged not fixed (§2.18 / D4.3):** `kernel/mem::Slab::alloc`
                 finds a free slot with an `O(slot_count)` scan of `in_use` rather
                 than an `O(1)` free-index. This is **not a live defect** — its
                 sole production caller (`kthread.rs` kthread-stack slab) uses
                 `slot_count == 1`, so the scan is O(1) today. **Trigger for the
                 rework:** if any large-`slot_count` consumer of `Slab` is
                 introduced, `Slab` must first gain an O(1) free-slot index (a
                 free-slot stack/head) so allocation does not become O(n) under
                 §26 load — that consumer's change carries the free-index rework.
               Then the original D2b-2b tail continues: the `CallEndpoint`-served
               `/System` file-read request loop on the parked store-service
               kthread + `driver_store_load`, delete the in-kernel single-pass
               `driver_autoload` it subsumes (§2.14), re-point the `-M virt`
               autoload vertical to the devmgr path. The parked-kthread/EMMC2
               interaction is a metal checklist (§0.9).
       - **D3 — `vcmailbox` IPC service driver + user-space `vl805`.**
         - **Prerequisite — server-side synchronous-IPC ABI — DONE (host-proven;
           gate pending).** A user-space service must answer *synchronous*
           `ipc_call`s, but the `CallEndpoint` (D2b-2a) was callee-only inside
           the kernel. Operator chose (b): add the first-class server-side
           primitive (not a convention over async ports). Landed: `abi-v1`
           syscalls **`call_create` (32) / `call_recv` (33) / `call_reply` (34)**
           (`lib/abi`, regenerated C header + `abi-sys` stubs); `kernel/ipc`
           `CallEndpoint` gains an `owner` + a size-bounded `recv_call(max_copy)
           -> RecvCall` (no lost request on a small buffer); `kernel/core`
           handlers (gate on `required_recv_caps` **and** owner, park on
           `SERVE_WAITQ`, wake `CALL_WAITQ`) + task-exit teardown
           (`callreg::unregister_owned_by`, so a dead server releases blocked
           callers fail-closed, §2.9); `lib/rt` wrappers. Host-tested
           (`kernel/ipc` + `kernel/core` round-trip + fail-closed paths); doc
           `docs/src/architecture/syscalls.md` rows 31–34. The production
           consumer is the `vcmailbox` service below (the `hw_tree_read`/`wait`
           staging shape, §2.3).
         - **Mailbox service + protocol + client — DONE (host-proven).** The
           `lib/abi::mailbox_ipc` wire protocol + well-known `MAILBOX_ENDPOINT`
           (`CAP_MAILBOX` send gate, id 25); the user-space `vcmailbox` service
           driver (`drivers/bus/mailbox/vcmailbox`: maps the mailbox MMIO/buffer
           from its node grants, serves the endpoint via `call_create`/
           `call_recv`/`call_reply`); `lib/drvrt::RtDriverHost::mailbox()` IPC
           client (so the existing `vl805` `MailboxChannel` driver runs in user
           space); the discovered mailbox node carries the doorbell + DMA
           requests; bind identity is one definition in `lib/vcmailbox`.
         - **Image install (the "D4" step) — DONE (host-proven).** `cargo xtask
           image` cross-compiles the `vcmailbox` driver PIE, converts it to an
           `rxe` (`USER_IMAGE_BIAS` + `SYSCALL_TABLE_HASH`), signs it as a
           `kind = UserSpace` `DriverManifest` (`CAP_MMIO_MAP`/`CAP_MEM_DMA`/
           `CAP_IPC_BIND_PRIVILEGED`, `lib/vcmailbox::BIND_KEYS`, kernel
           driver-signing seed), and installs it into the read-only
           `/System/Drivers/bus_mailbox/vcmailbox/Run` store. `tools/mkimage`
           stays pure (`build_rpi_image`'s `drivers` seam plants bytes only); the
           ELF→`rxe` converter and signer are the shared `tairix_itest_harness`
           definitions the kernel `build.rs` uses (§2.2); the store-planting
           routine is the single `tairix_drv_fs_arxfs::plant_nested_file`.
           Host-tested: mkimage plants and reads the bundle back from the
           read-only `/System` store; the image builds end to end in the CI image
           gate.
         - **Metal autoload — install + scan confirmed; load fixed, re-verify
           pending.** On a real Pi 4 the kernel store scan finds and accepts the
           installed bundle as an autoload candidate (`id=4042 drivers=1`,
           `id=7030 .../bus_mailbox/vcmailbox/Run`), but the user-space `devmgr`
           never issued the matching `StoreRequest::Load` and was relaunched in a
           loop. Root cause (a §24.1 fixed-capacity defect): `devmgr` read the
           discovered hardware tree into a hand-picked 64 KiB buffer
           (~114 `HwNode`s) — ample for QEMU `virt` but far too small for a real
           Pi's full firmware tree — so `hw_tree_read` returned `BufferTooSmall`,
           the reactive loop treated it as fatal, and `init` relaunched the
           service. **Fixed:** `devmgr` now owns a *growable* tree buffer
           (`service::read_tree_growing`) that doubles and retries on
           `BufferTooSmall` (grow-before-fail, §24.1; the tree is a discovered
           capacity, not a ceiling), so the whole tree is read and the mailbox
           node is matched and loaded. Host-proven (new `service` growth tests +
           a `run`-over-an-oversized-tree end-to-end test). A second metal run
           then confirmed `devmgr` reaches the matched node and issues the
           `Load`, but the in-kernel store-load gate refused it with
           `id=7006 capability escalation`: the signed `vcmailbox` manifest
           requests `CAP_IPC_BIND_PRIVILEGED` (to bind its restricted-sender
           `MAILBOX_ENDPOINT`), which the autoload gate's delegatable superset
           `unlock_service::autoload_caps()` did not carry, so
           `requested.is_subset_of(caller_caps)` failed. **Fixed:**
           `autoload_caps()` now also inserts `CAP_IPC_BIND_PRIVILEGED`
           (alongside `CAP_INPUT_INJECT`/`CAP_IRQ_BIND`) so a signed bus
           *service* driver can be granted it; the per-driver manifest∩superset
           intersection still binds (§5.2 / §18.3 / §4 — no ambient authority).
           Host-proven (rewritten `autoload_caps` unit test); metal confirmed
           the load succeeds (`id=7001 driver loaded .../bus_mailbox/vcmailbox/Run`)
           and the USB keyboard works. Two §20/§2.16 log-spam defects then made
           the `Root passphrase:` prompt look unresponsive (not an input-path
           bug): `devmgr` re-matches the whole tree snapshot on every generation
           advance (§18.4), and each unmatched node emitted an `Info`
           `NODE_UNBOUND` line over the Pi's flow-blocked debug UART
           (~116 ms/line) — with ~120 mostly-driverless device-tree nodes this
           starved the keyboard report pump for tens of seconds. **Fixed in two
           parts:** (1) per-node decision memory
           (`autoload::ReportedNodes`/`NodeReport`) so a node is logged only on
           its first decision and on a *change* (`Unbound`→`Bound`), making a
           settled re-evaluation silent; (2) `NODE_UNBOUND` is logged at `Debug`
           (not `Info`) in both the user-space `match_and_load` and the
           kernel-side `DeviceManager::autoload` sibling (§2.2), so the routine
           unbound nodes are dropped in O(1) by the default `Info` level filter
           *before* any `log_emit` syscall — even on the first pass — while still
           logged with their stable id when diagnostics are enabled (§18.4).
           `NODE_BOUND`/tie/load-failure stay visible (`Info`/`Warn`). Host-proven
           (`a_reaction_does_not_relog_an_unchanged_unbound_node`; the four
           unbound-asserting tests lower the level to observe the `Debug` record).
           A third, same-class spam source then surfaced on metal: the in-kernel
           keyboard report pump emitted its periodic liveness *heartbeat*
           (`usb_keyboard` id `4131`) at `Info`, ~32× at a ~160 ms cadence, and
           the pump emits it *synchronously* — over the ~116 ms/line debug UART
           that blocked the pump for the bulk of the `Root passphrase:` window,
           so typed keys were slow/dropped and the first attempt failed. **Fixed:**
           the heartbeat is now `Debug` (the one-shot first-report stays `Info`,
           the pump error `Error`), so it is filtered in O(1) on a default-`Info`
           boot and never blocks the pump; it is still captured when diagnostics
           lower the threshold. Host-proven (`pump_diagnostics_emits_a_bounded_heartbeat`
           lowers the level to observe the `Debug` record).
           The structural defect behind all three was a *synchronous* serial
           write: the log sink pushed each line to the UART byte by byte,
           blocking the calling task — single-CPU, the whole core — for the
           line's transmit time, so *any* task's logging (devmgr's flood via
           `log_emit`, a kthread heartbeat, program console output) starved the
           keyboard pump. **Fixed:** every port buffers **all** console output —
           the diagnostic log sink *and* `write_console_bytes` (the
           `stream_write`/`ConsoleWrite` backing) — through the shared
           `lib/conout` engine, so the two share one ordered stream on the wire
           and a producer copies its bytes and returns.
           **Draining never blocks the CPU on this port; the TX interrupt +
           `wfi`-wake do it in the background.** One non-blocking step (push
           what the FIFO accepts now, then arm `TXIM`) is shared by the
           producer, the transmit ISR and the dispatch loop (`pump_tx`, §2.2);
           `poll_interrupts` and the idle `wait_for_interrupt` call it and then
           `wfi` plainly — no per-byte spin. The backlog then flows at the
           UART's real rate through the console TX interrupt
           (`serial::service_uart_tx_irq`), which wakes the `wfi` the moment the
           FIFO has room, and tickless idle (§17.1) resumes. The TX line is
           routed+unmasked at GIC bring-up with device sources masked at reset
           (additive, §2.17); the ISR reads the masked status
           (`ConsoleModel::tx_interrupt_fired`/`rx_interrupt_fired`) so it
           drains receive bytes only when RX actually fired — the passphrase
           FIFO poll keeps its bytes while RX stays masked (§5.4, fail closed).
           The TX register policy is in `ConsoleModel` for both PL011
           (`UARTIMSC.TXIM`/`UARTMIS`/`TXIC`) and mini-UART (`IER`/`IIR`),
           host-tested. Boot beacons stay on the direct lock-free `putchar`
           path (MMU-off, must trace immediately), and the panic bridge
           `flush_serial_blocking`s buffered context out before parking.
         - **The output queue is shared by every port, not aarch64 glue.** An
           earlier revision kept a private *byte* ring here, reasoning that the
           other ports did not share the flow-blocked-PL011 defect. That was
           wrong twice over. A byte ring drops individual **bytes**, truncating
           a line and letting the next line's bytes fill the gap — the reported
           output corruption — with no accounting, while `plans/SYSLOG.md`
           requires a trusted loss record and forbids silently dropping an audit
           record (this sink is installed as *both* log and audit sink). And the
           other two ports had **no serialisation at all**, so two CPUs/harts
           logging concurrently interleaved mid-line, x86_64 additionally
           spinning *unboundedly* on its transmitter (§2.1/§2.23). All three
           ports now share `lib/conout` (§2.2/§2.21): whole-line frames,
           severity-ordered shedding (a record may evict a newer, less severe
           record; program output and the in-flight frame never), loss counted
           and reported on the wire where the gap is (`CONSOLE_OUTPUT_DROPPED`
           18001), one bounded transmit wait, and one `IrqSafeSpinLock` masking
           discipline through each port's single masking primitive
           (`arch/<target>/irqmask.rs`, also de-duplicated out of the kernel
           binary). Each port supplies only its device: PL011/mini-UART
           registers with a completion interrupt here; 16550 port I/O
           write-through on x86_64 (its interrupt is unmasked only at session
           start, after the whole boot log); the firmware console call on
           riscv64, whose interface reports neither readiness nor completion.
         - **Remaining (metal-only, §0.4):** confirm serial output now flows
           smoothly at the UART's real rate *throughout* boot and idle (no
           chunk-then-pause) and the `Root passphrase:`/login prompts stay
           responsive, the CPU no longer being starved by the serial drain.
           Separately, the in-kernel `pcie_brcm` **inbound-window read-back**
           diagnostic (`4119`/`4120`/`4111`, `usb_keyboard.rs`) measures ~4.3 s
           of MISC-register MMIO on metal *after* the link is trained (it was
           masked by the old synchronous-serial timing); this is intrinsic
           BCM2711 bus latency, not the serial drain — once the bring-up is
           confirmed good these debug-only read-backs are candidates to delete
           (§2.14). `login` correctly parks on `users_db_wait` (woken by
           `users_db_wake` on db install); the 5 s metal cadence was just the
           wait timing out while the operator was still typing. Then the
           **vl805 user-space migration** (run the firmware-reload driver over
           `host.mailbox()`)
           with retirement of the in-kernel scaffold
           (`bring_up_keyboard`/`KernelMailboxChannel`, §2.14/§2.17) — the
           prompt's "D5". The scaffold stays the live keyboard path until that
           flip.
       - **D4 — recursive, user-space hardware discovery.**
         - **D4-keystone — `hw_emit_node` + `CAP_HW_EMIT` — DONE (host-proven).**
           The ABI+kernel mechanism a user-space bus driver uses to publish a
           discovered child into the live hardware tree so `devmgr` autoloads
           the matching driver in turn. `abi-v1` syscall **`hw_emit_node`**
           (no. 37, gated on new **`CAP_HW_EMIT`** (27), audited) decodes the
           emitted `HwNode` fail-closed and admits it **only** when every
           requested `HwResource` is covered by one of the calling task's own
           minted grants (`HwResource::covers` — the security spine, §4 no
           ambient authority / §18.3), then appends it to `HwTreeStore` via the
           new `HwTreeSource::publish`, bumping the generation that wakes the
           reactive `devmgr` loop. Wired end to end: `tairix_rt::hw_emit_node`,
           `RtDriverHost::emit_node` (over the `GrantSyscalls` seam),
           `tairix_sys_hw_emit_node` C stub + regenerated header. Host-tested
           (`covers` accept/reject per kind; kernel handler no-store / wrong-len
           / uncovered→`PermissionDenied` / covered→published; drvrt
           `emit_node` forward + refusal). The bootstrap floor (§18.6) seeds
           only the nodes needed to reach the store; everything below a
           discovered bus is published by that bus's user-space driver.
         - **D4-covers — `BusWindow`→`Mmio` bridge-BAR coverage — DONE
           (host-proven + whole gate).** The shipped `HwResource::covers`
           security spine handled only same-kind containment, so it wrongly
           fail-closed-*rejected* the central recursive-PCI(e) case the
           user-space consumers need: a bus driver holds its host bridge's
           outbound window as a `BusWindow` grant, but an enumerated child's
           register BAR is an `Mmio` window resolved to a CPU address inside
           that bridge window. `covers` now decides per kind and admits the one
           cross-kind pairing — a `BusWindow` parent covers an `Mmio` child by
           CPU-side containment, never wider (§4 — the child only receives a
           window the bridge already owns). Restructured to tuple-match
           `(parent_kind, child_kind)` with a fail-closed default; all
           same-kind rules unchanged. Host-tested in `lib/abi`
           (`covers_lets_a_bridge_window_cover_a_child_bar_inside_it`:
           accept contained BAR, reject below/past-end/overflow, reject the
           non-symmetric `Mmio`→`BusWindow` and `BusWindow`→`Port`/`Irq`) and
           through the real syscall consumer in `kernel/core`
           (`hw_emit_node_covers_a_child_bar_under_a_bridge_window`). Docs:
           `drivers/hardware-detection.md` recursive-discovery section.
         - **The user-space bus-driver chain (the D5 flip) — DONE.** The
           in-kernel `bring_up_keyboard` composition is now a reactive chain of
           autoloaded user-space driver **binaries** (the `usb_kbd`/`virtio_kbd`
           pattern) over the rt-backed `DriverHost`. The hardware forbids the
           plan's earlier "`bus_usb` emits a HID node, `usb_kbd` binds it" split:
           BAR assignment needs the live trained `PciBus` and the `Xhci`
           controller object cannot cross a process boundary, so `usb_kbd` maps
           the BAR *by address* and does the whole xHCI bring-up + enumerate +
           pump itself.

           A user-space driver bin (a `drivers/*` or `userland/*` crate) may
           depend only on `lib/*` (deps-check `layer_allows`), and it cannot
           share a crate with the kernel-linked lib. The genuinely-shared,
           board-neutral mechanism therefore lives in `lib/*` — `lib/pci` (PCI
           config/enumerate/BAR/mechanism, `mechanism_one/ecam/brcm`, the
           `Bus`/`VirtioPciBus`/`MsixBus`/`PciBus` seams, and the shared
           `find_function_by_class`/`assign_and_map_bar`/`bus_to_cpu_phys`
           locate primitives) and `lib/usb` (the bus-agnostic xHCI protocol +
           `XHCI_COMPATIBLE`/`XHCI_BAR_INDEX`/the `SlabBank` DMA bank), the
           `lib/usb`↔`drivers/bus/usb` precedent. Each device's **own** logic,
           by contrast, lives **in its driver crate** as a host-testable `lib`
           target the `Run` binary links (§2.22), since a driver above the
           §18.6 floor has no charter-legal non-driver consumer for a `lib/*`
           device-support crate.

           **Status: DONE.** The chain is five autoloaded user-space pieces,
           each its own job, no `drivers/*→drivers/*` edge, ordering enforced by
           the `hw_emit_node` chain, least-privilege caps:
           1. `FdtDiscovery` seeds the RC node (`brcm,bcm2711-pcie`,
              Mmio+Dma+BusWindow) + mailbox node (`brcm,bcm2835-mbox`) into
              `HW_TREE`.
           2. **`drivers/bus/pcie_brcm`** (`CAP_MMIO_MAP`+`CAP_HW_EMIT`): binds
              the RC node, trains the link, builds the `PciBus` over `lib/pci`,
              assigns+enables the VL805 BAR, and emits node A
              `pci(1106,3483,0C0330)` carrying `Mmio(bar_cpu_phys,bar_len)` +
              the inbound-DMA viewport grant. The BCM2711 bring-up engine
              (`BrcmPcieRc`, the `regs`/`wiring`, `BIND_KEYS`) is its own `lib`
              target.
           3. **`drivers/bus/mailbox/vcmailbox`** serves `MAILBOX_ENDPOINT`; the
              property-message layout is the genuinely-shared `lib/vcmailbox`
              (its other consumer is the aarch64 framebuffer boot console, so it
              stays in `lib/*`, §2.22).
           4. **`drivers/bus/usb/vl805`** (`CAP_MAILBOX`+`CAP_HW_EMIT` **only**):
              reloads firmware over the mailbox IPC, then emits node B
              `compatible("usb,xhci")` forwarding the BAR+DMA grants. The VL805
              firmware policy (`reload_firmware`/`probe_firmware_revision`,
              `BIND_KEYS`, `build_xhci_node`/`reload_firmware_and_publish`) is
              its own `lib` target. Firmware-before-bring-up holds by
              construction (node B does not exist until vl805 runs).
           5. **`drivers/input/usb_kbd`** (`CAP_MMIO_MAP`+`CAP_MEM_DMA`+
              `CAP_INPUT_INJECT`): binds node B (`tairix_hid::KEYBOARD_BIND_KEYS`,
              exact `compatible("usb,xhci")`), maps the BAR, brings up xHCI
              (the `Xhci` object can't cross a process boundary, so it does the
              whole bring-up), enumerates, and pumps key edges.

           The kernel owns every emitted node's identity (`hw_emit_node`
           assigns a unique id and the caller's own loaded node as parent,
           fail-closed; per-resource `HwResource::covers` coverage, including
           the `BusWindow`→`Mmio` bridge-BAR case). `hw_remove_node` (no. 38,
           `CAP_HW_EMIT`, audited) is the mirror that retires a published node +
           subtree. `driver_catalog::IN_KERNEL_DRIVERS` is the storage bootstrap
           floor **only** (virtio-blk + EMMC2, §18.6); the in-kernel keyboard
           scaffold is deleted (§2.14) and the four bundles are installed into
           the image `/System/Drivers/` store, signed with the kernel's
           driver-signing seed. No `-M virt` Pi-USB vertical exists (§0.4); the
           live enumerate→emit→autoload chain + a keystroke is the on-metal
           acceptance item (§0.9).

---

## Stage 5 — Filesystem

**Dependencies:** Stage 4 (`Filesystem` trait + a block driver).

**Deliverables**
- `drivers/filesystem/arxfs`: native FS, copy-on-write, ACL + capability
  gates per inode, journaled, POSIX-compliant (latest standard targeted).
- `drivers/filesystem/ext4`: read/write driver (uses upstream-audited parser
  where possible; otherwise implemented in-tree with tests).
- `drivers/filesystem/fat32`: read/write (for EFI system partition and SD
  cards).
- VFS layer in `kernel/core` (path resolution, mount table, permission
  enforcement via `kernel/sec`).
- Enforcement of the on-disk layout defined in `AGENTS.md` §16: the OS
  never authors the legacy POSIX top-level names (`/etc`, `/home`, …) —
  the default root template, the image builder, and the installer create
  only `/System`, `/Users`, `/Apps`, `/Storage`. The VFS does not police
  a user's own request to create such a name; a top-level create is
  governed by ordinary write permission on `/`.

**Tests**
- POSIX FS test suite (`pjdfstest`-equivalent) run under QEMU.
- ACL + capability gate tests: a user without `CAP_AUDIT_READ` cannot read
  a file marked as such, even with mode 0644.
- Crash-consistency tests for `arxfs` journal.
- Layout-enforcement tests: the default layout exposes exactly the four
  top-level directories; a user with root write permission may `mkdir
  /etc` (the VFS reserves no error for the name); `/System` is read-only
  at runtime except for the two writable paths listed in §16.2.

**Docs**
- `docs/src/filesystem/{overview,arxfs,ext4,fat32,permissions,layout}.md`
  (the new `layout.md` mirrors `AGENTS.md` §16).

**Status: complete.**
- Arch-neutral **VFS** in `kernel/core/src/fs/` (`path`, `perm`, `mount`,
  `vfs`): absolute-path-only parsing rejecting relative/`.`/`..`/NUL/over-long
  components, the §16.1 four-entry root template; the §5.3
  permission model (mode bits + ACL + per-inode capability gate) via one
  fail-closed `Metadata::authorize` (never branches on `uid == 0`);
  longest-prefix `MountTable` with read-only `/System` (writable `Logs`/
  `Settings`).
- Filesystem drivers: `arxfs` (native COW, journaled), `ext4` (read +
  checksummed/`64bit`/`metadata_csum` validated against `mke2fs`/`e2fsck`,
  first-party crc32c/crc16), `fat32`, and `adfs` (read/write across every
  Acorn `FileCore` format — S/M/L/D old map, E/F new map, E+/F+ big
  directories, old- and new-map hard discs — validating every on-disc
  checksum, with RISC OS load/exec/filetype/datestamp/attribute metadata
  surfaced through the shared `lib/fsmeta` `acorn.*` keys and a
  corruption test suite plus a registered `fuzz_mount` harness). Each
  ships a first-party `format` (no `mkfs` shell-out, §12) and returns
  `NoSpace`/`Errno::NoSpace` on exhaustion.
- Tests: `arxfs` journal crash-consistency soak (seeded, old-or-new
  recovery), end-to-end `arxfs`-over-virtio_blk QEMU vertical (fixture
  authored by `ARXFS::format` itself, §2.2), the `pjdfstest`-equivalent
  `posix_fs_suite` over the real driver + VFS, and `fs_soak` (`cargo xtask
  fssoak`) exercising every formatter over a ≥ 1 GiB `RamBlock`.

---

## Stage 5 follow-up — ARXFS (native on-disk format evolution)

**Dependencies:** Stage 5 (the VFS policy layer and the frozen
`Filesystem*` traits) and `lib/crypto`.

**Goal.** Grow the native filesystem to the full ARXFS design — copy-on-write,
always-encrypted, checksummed, compressed, deduplicating, SSD-aware,
recoverable — as **one** on-disk version (no `v1`/`v2` pair) behind the frozen
`Filesystem*` traits. One mandatory profile (every feature on, not tunable);
first-party codec (no external zstd, §2.12); crypto via `lib/crypto` only. Spec:
`docs/src/filesystem/arxfs-spec.md`; user docs: `docs/src/filesystem/arxfs.md`.

**Status: all stages complete — ARXFS v1 is done.** The COW `arxfs` driver
replaced the old journaled one outright (self-identifying block headers,
four-slot superblock ring, transaction root + inline commit, COW inode map)
and grew through spec Stages 2–12 into: COW B-trees, keyed-MAC + mirrored
metadata, at-rest encryption, per-record integrity, mandatory compression +
dedupe, online scrub, offline check/rescue, safe TRIM/discard, plus the
fuzz/crash-replay/corruption-injection suites. Also done: always-on **sparse
files** (metadata-only holes detected pre-hash/dedupe/compress, spec §19) and
**255-byte directory names** (263-byte slot, ext4 charset rules,
case-sensitive) with online **grow** (shrink rejected, spec §13). Passes unit
tests, the 1 GiB `fssoak`, the POSIX suite, the arxfs-over-virtio_blk QEMU
vertical, and the `fuzz_mount`/`fuzz_compress` harnesses. Per-stage legend:
spec §18.

Compression is **cluster-granular** (on-disk format v2, spec §6/§10): an
aligned 16-block cluster stores as one compressed extent in strictly fewer
physical blocks (`drivers/filesystem/arxfs/src/cluster.rs`), so the savings
are real freed space; a single-block record always stores raw (inside a fixed
1:1 block a compressed frame frees nothing, so no CPU is spent where no block
can be freed). Partial overwrites decompose a cluster back to per-block
records; reflinks share clusters whole; seeks stay one extent-tree descent.

---

## Stage 5 follow-up — ARXFS extended file metadata (`plans/ARXFS-METADATA.md`)

**Dependencies:** Stage 5 follow-up (ARXFS v1) and `lib/crypto`. Design brief:
`plans/ARXFS-METADATA.md`; spec: `docs/src/filesystem/arxfs-spec.md` §21 +
`docs/src/filesystem/metadata-registry.md`.

**Goal.** Give every ARXFS inode a general-purpose, namespaced extended-
attribute store and use it to preserve foreign-filesystem per-file metadata
(Acorn/RISC OS, Amiga, Atari, classic Mac) across a copy — interoperability
with foreign data, not TAIRiX self-compatibility (§2.13). One shared definition
in `lib/fsmeta` (grammar, `AttrSet`/`AttrEntry`, preset registry with checked
`Time64` conversions), consumed by ARXFS, the foreign-FS drivers, and the
copy/archive tools (§2.2).

**Design decisions (load-bearing).**
- Attribute set = one self-identifying, encrypted, mirrored COW metadata block
  (`BlockType::Attr`) reached from the inode `attr_root`, reusing the Stage-3
  authenticated repair-on-read path (no second integrity/crypto path).
- Fixed security bounds (`AGENTS.md` §24.4, not capacities): `KEY_MAX` 255,
  `VALUE_MAX`/`TOTAL_ATTR_BYTES` 3072, `ATTRS_PER_INODE` 32. `VALUE_MAX` is
  sized to one 4 KiB metadata block; a larger fork is a *named stream*, not an
  attribute. A set that overflows a smaller block fails closed (`NoSpace`).
- Versioned `FilesystemAttrs` ABI (separate trait, never a widening); privileged
  `system`/`trusted` namespaces gated by the VFS (capability introduced with its
  enforcement point, §5.2 — none minted here).

**Status: foundation + syscall surface done.** Delivered: `lib/fsmeta`
(grammar + AttrSet + preset registry + fuzz harness), the `FilesystemAttrs`
ABI, and the ARXFS attribute store (encrypt/decrypt, COW read/write,
free-on-remove with correct allocation-map accounting, reflink copy) with driver
tests (round-trip/remount, case-sensitivity, unknown-namespace + oversize +
block-overflow fail-closed, encryption at rest, read-only refusal,
crash-atomicity replay, no-leak, reflink independence, acorn preset
round-trip). The userland surface is live: the `fs_attr_get`/`fs_attr_set`/
`fs_attr_list`/`fs_attr_remove` syscalls (84–87, `CAP_FS_ACCESS`, mutations
audited; wire bounds `FS_ATTR_KEY_MAX`/`FS_ATTR_VALUE_MAX` in `lib/abi`,
aliased by `lib/fsmeta`) flow dispatcher → `MountedFilesystemService` →
`Vfs::*_attr_via_secured` → `DelegatedFs`, where the shared key grammar is
validated, ordinary namespaces follow the node's own read/write permissions,
and the privileged `system`/`trusted` namespaces are refused and hidden from
listings (their capability still arrives with its first holder, §5.2). A
driver declares support through the `FilesystemAttrsProvider` facet
(`ARXFS` serves it; `CachedFs`/`GroupMappedFs` forward with cache
invalidation and mapped authorisation; ext4/FAT32 answer the typed
`Errno::NotSupported`; the new `Errno::NoData` is the absent-attribute
answer). `lib/rt` wrappers, `tairix_sys_fs_attr_*` C stubs, and the regenerated
headers complete the ABI; `fstree`'s `a` attributes editor is the first
caller.

**Remaining (staged, not yet built):**
- `cp`/`mv`/desktop/archive preserve-metadata tooling (the §6.2/§6.3 copy
  contract and a `getattr`/`setattr`-style CLI).
- The resource-fork *named-stream* content path for values above `VALUE_MAX`.
- Per-family foreign-FS driver wiring (lands with the ADFS/Amiga/Atari/Mac
  drivers, which do not yet exist).
- Snapshot send/receive carrying the attribute set (with `plans/ARXFS-SNAPSHOT.md`).

---

## Stage 5 follow-up — ARXFS FEC and multi-device redundancy (`plans/ARXFS-FEC.md`)

**Dependencies:** Stage 5 follow-up (ARXFS v1). Staged plan:
`plans/ARXFS-FEC.md` (stages FEC0–FEC20).

**Goal.** Always-on forward error correction and multi-device redundancy for
ARXFS: local RS(8+2) media repair on a one-device pool; replication or
topology-selected RS(k+1)/RS(k+2) across distinct whole-device failure
domains; a semantic "survive N whole-device failures" protection floor (never
raw `k+m`); online add/remove/replace/rebalance/protection changes with
second-failure-safe COW recovery; and one capability-gated administration
service fronted by CLI and curses-TUI command apps. Devices are reached only
through the existing storage-discovery path (`blkio` block-service endpoints
on storage-class hardware-tree nodes, consumed by `drivers/storage/volmgr`).

**Status: planned.** Design, invariants, staging, and acceptance live in
`plans/ARXFS-FEC.md`; no implementation has landed.

---

## Stage 5 follow-up — filesystem path-walk performance (uncached first-access cost)

TAIRiX runs its filesystems uncached by design (first-access speed is the
product requirement), so every block read is a device round-trip plus a
whole-block HMAC verification. That makes redundant reads a first-order
cost: a measured depth-4 `fs_open` walk performed 38 block reads, and a
directory listing was O(n²) (each `read_dir(index)` rescanned from entry
0 while `du`-style tools then re-opened and re-statted every child, a
full path walk per child).

**Done (landed with the cursor-listing change):**
- `FilesystemRead::read_dir` takes an opaque resume cursor (`getdents`
  `d_off` model): a full listing is one bounded scan; a stale or
  arbitrary cursor is bounds-checked and fail-closed. All four
  implementations (arxfs, ext4, fat32, in-RAM mock) resume in O(1).
- The driver `DirEntry` carries the child's full `NodeInfo`, the service
  `ReaddirEntry` and the wire record carry `size`/`allocated`, and `du`
  sums a directory from the one listing (one open + one readdir per
  directory instead of `n` open/stat/close round-trips).
- The delegated listing fails closed on a non-advancing driver cursor,
  and arxfs regression tests pin the O(1)-resume read cost and the
  per-entry metadata.
- **Repeat-access cost is now served by the clean, rebuildable
  filesystem cache** (`kernel/core::fs::CachedFs`, see the SMARTRAM
  section below): warm stat/lookup/security/dirent/data reads never
  reach the driver, so the per-component walk cost above is paid once
  per (unchanged) inode, not once per operation. First-access cost is
  untouched and stays governed by the items below.

**Remaining (staged, each its own change):**
- **Descriptor→node binding.** An open descriptor stores the resolved
  node + access grant, not the path string: today every `fs_read`/
  `fs_write`/`fs_stat`/`fs_readdir` on an fd re-resolves the full path
  through the secured VFS. POSIX semantics (check at open, use the
  grant thereafter) require the descriptor to hold the authorised node;
  revocation semantics must be designed with it.
- **Single-descent secured walk.** The delegate's per-component
  `lookup` + `node_info` + `security` triple re-reads the same inodes
  via separate B-tree descents (~9 reads per component measured on
  arxfs); a combined driver resolve step should read each inode once
  (~2× fewer reads per open).
- **Per-block MAC cost on target hardware.** Every metadata read pays an
  HMAC-SHA256 over the whole block in software (Pi 4 has no ARMv8
  crypto extensions). If on-hardware measurement shows the MAC (not the
  device round-trip) dominating after the walk fixes, evaluate a faster
  audited keyed hash for the block authenticator (an on-disk format
  change; pre-release, so it evolves in place).

---

## Stage 5 follow-up — demand-paged file mappings (`file_map` / `file_unmap`)

**Status: landed (aarch64, riscv64, and x86_64 all fault-resolving),
including the QEMU end-to-end verticals
(`tests/integration/file_map_qemu_aarch64` / `…_riscv64`). One staged
remainder: kill the task (not the CPU) for a user instruction-fetch
fault, below.**

`abi-v1` syscalls 75/76 give a program a read-only, demand-paged private
mapping of an open file (the `mmap(2)` shape): `file_map` reserves address
space only (out of the per-task file window `user_windows` splits above the
heap window) and records the mapping-time identity; each page is backed on
first access by the user-fault resolver (`DispatchHook::resolve_user_fault`
→ `KernelSyscallHandlers::resolve_file_fault`), which reads the covering
page through the secured VFS under that identity and maps it read-only,
never executable. An unresolvable fault (wild access, page at/past
end-of-file, read error, OOM) terminates the faulting task with exit 139 —
reclaiming exactly what `exit`/signal-kill reclaim — never the machine.
`file_unmap` releases sparsely (resident frames zeroed on free) and the
shared `AddressSpaceBytes` accounting covers both map families. Full design
and per-port state: `docs/src/architecture/memory.md` §7f/§7o. First
consumer: `fstree`'s viewers (`RtFs` mapped reads with streamed fallback).
The x86_64 resumable `#PF` entry (save → resolve under the timer path's
`swapgs` convention → restore → `iretq` retry), copy-path fault
resolution (`KernelSyscallHandlers::copy_in_user`, so an untouched
mapping works as a syscall buffer), and the `TaskFaultKilled` audit event
are landed. Every port offers every user-mode **data** fault (read *and*
write) to the resolver seam with the port-attested `write` verdict
(`UserFaultResolveFn(addr, write)`); the one shared hook policy resolves
only a read inside a live file region and terminates the task for any
write or unresolvable read — closing the defect where a user store to a
read-only mapping (or any wild write) bypassed the resolver and halted
the whole CPU. The QEMU verticals prove the path live on both boards
through the production `KernelDispatchHook` (demand-fault + zero-fill
verification, mapping survives `fs_close`, untouched mapped page as an
`fs_open` buffer, sparse unmap, wild-read and read-only-store children
reaped at 139 through production `spawn` + `wait`).

Remaining work — **user instruction-fetch faults kill the task.** A
user-mode *instruction* fault (a wild jump to unmapped/kernel-only
memory) is still never offered to the resolver and falls to the port's
fatal path, which halts the CPU when no `FaultHandlerFn` is installed —
the same denial-of-service shape the write-fault fix closed for data
aborts. Extend the per-port offer gates (aarch64 lower-EL instruction
aborts, riscv64 `SCAUSE_INSTRUCTION_PAGE_FAULT` from U-mode, x86_64
`PF_ERR_INSTR` with `U/S` set) to route through the same task-kill
policy (never a resolution — file mappings are never executable), with
per-port classifier regression tests and a QEMU jump-wild child case
added to the file-map verticals.

## Stage 6 — Userland Foundations

**Dependencies:** Stages 2–5 sufficient for at least one platform.

**Deliverables**
- `userland/system/init` (PID 1): service manager, dependency-ordered start,
  reaper, capability granting from manifests.
- `userland/shell/elsh`: POSIX-ish shell with job control and a small builtin set.
- `userland/session/login`: text login that authenticates against `kernel/sec`
  and spawns a shell or a graphical session. Which session runs is system
  policy, never a per-login prompt: the desktop whenever `os.loginType` is
  `graphical` — the default — *and* a graphical session is available,
  degrading to the account's shell otherwise; a shell user starts the
  desktop on demand with the `desktop` command app.
  The console presentation is the full-screen curses view
  (`tairix_login::view::CursesView` over `lib/curses`): top bar with
  hostname/OS-version/clock, centred bordered login box, red running
  failed-attempt count, bottom bar with memory/tasks/users/load figures
  from `sysinfod` (`LOAD_AVERAGE` — the kernel `LoadTracker` tickless
  EWMA over the `IntrospectDomain::LoadAverage` primitive, whose runnable
  census excludes the observing broker's own task so an idle machine reads
  ~0.00 — plus `SYSTEM_IDENTITY` and the `CAP_SYSINFO_KERNEL`-gated memory
  stats); a refused figure renders `--` and never blocks a login, and a
  raw-mode failure refuses the password read (never echo a credential).
  The bars and clock refresh every 5 s through the `stream_read`
  `timeout_ns` bound (a one-shot kernel park, never a poll); the username
  field is bounded at `tairix_users::MAX_USERNAME_LEN` (32, refused whole
  beyond it) so it cannot overflow its one-line box; and every hidden
  field renders the shared `tairix_vt::secret` `[input active...]` marker,
  its dots driven by the shared `SecretIndicator` one-second timer cadence
  (never a keystroke, so the marker reveals nothing about how much was
  typed).
- Core CLI utilities (`ls`, `cp`, `mv`, `rm`, `cat`, `ps`, `mount`,
  `chmod`, `chown`, `useradd`, `groupadd`, `setcap`, `getcap`,
  `sysinfo`). Each utility is its own small crate under `userland/apps/`.
  `ps`, `mount`, and `sysinfo` are clients of the System Information API
  defined in `AGENTS.md` §16.6 (`lib/abi/src/sysinfo.rs`); they do **not**
  read a `/proc`-style virtual filesystem.
- `lib/abi/src/sysinfo.rs`: typed, versioned, capability-gated request /
  response types for the System Information API (§16.6). Frozen on
  release; new queries ship as `sysinfo-v2`.
- `userland/system/sysinfod`: user-space system service that serves the
  API. Installed to `/System/Services/sysinfod`.
- Application-bundle loader in `kernel/core` (or a user-space service
  invoked by `init`) that recognises `/Apps/<Name>.app/` bundles per
  `AGENTS.md` §16.5: parses and verifies the signed `AppInfo`
  manifest, computes the granted capability set as the intersection of
  the user's grants and the manifest request, and refuses bundles whose
  top-level layout deviates from the fixed set.
- Dynamic loader policy: shared-library references resolve only against
  the calling bundle's own `Libraries/` directory and `/System/Libraries/`
  (§16.4). Any other path is a load-time error.

**Tests**
- Integration tests: boot to login, log in, run each utility, exercise
  permission denials.

**Docs**
- `docs/src/userland/{init,login,shell,utilities}.md`.

**Status: complete.**
- System Information API (`lib/abi/src/sysinfo.rs`, §16.6): `sysinfo-v1`
  versioned/frozen query registry (six queries: self/global process list,
  kernel memory stats, hardware tree, system identity, uptime), each with a
  required capability (`CAP_SYSINFO_GLOBAL/KERNEL/HW` added) under the §9
  hash discipline; served by `userland/system/sysinfod`.
- `userland/system/init` (PID 1, dependency-ordered service manager + reaper +
  manifest capability granting), `userland/shell/elsh` (POSIX-ish, job
  control), `userland/session/login` (the desktop when `os.loginType` is
  `graphical` — the default — and a graphical session exists; the
  account's shell otherwise).
- Core CLI utilities each as their own `userland/apps/` crate (`ls`/`cp`/`mv`/
  `rm`/`cat`/`ps`/`mount`/`chmod`/`chown`/`useradd`/`groupadd`/`setcap`/
  `getcap`/`sysinfo`); `ps`/`mount`/`sysinfo` are sysinfo-API clients (no
  `/proc`).
- App-bundle loader (signed `AppInfo` verification, granted caps = user ∩
  manifest, fixed `.app` layout enforced, §16.5) and the dynamic-loader policy
  resolving only the bundle's `Libraries/` + `/System/Libraries/` (§16.4).
- User-account database (`lib/users`): the `/System/Security/Users`
  `users-v1` format (full §5.1 identity incl. shell of choice and the
  `CAP_*` grant ceiling), PBKDF2-HMAC-SHA256 password records over
  `lib/crypto`, fail-closed bounded parsing (fuzzed), and timing-equalised
  authentication; `login`'s production `Authenticator`
  (`UsersAuthenticator`) verifies against it. Image profiles
  (`tools/mkimage` `--profile debug|installer`) seed the debug
  `root`/`root` test account or, for the installer image, none. The
  kernel's boot-time root-volume read path
  (`tairix_kernel_core::users::load_users_db`) loads the database off the
  mounted root volume through the §5.3-checked VFS delegation, audited
  and fail-closed (proven on `-M virt` by the `users_db_qemu_aarch64`
  vertical). The kernel-neutral *install* seam is wired:
  `load_users_db_source` shares that read/parse/audit path (§2.2) but
  retains the canonical `users-v1` text in a `HeldUsersDbSource` (zeroed
  on drop §4, redacted `Debug`), and `BootInfo::with_users_db` installs
  the `Box::leak`'d holder so `run_phases` threads it into the production
  `KernelDispatchHook` (default fail-closed `NULL_USERS_DB`). The
  arch-neutral unlock + mount + load composition that *produces* that
  driver is wired (`tairix_kernel::root_mount::unlock_root_and_load_users`,
  `plans/PI.md` P11 Chunk A — host-proven): given the on-FAT `root.unlock`
  descriptor, the typed passphrase, and the encrypted root block device it
  derives the volume key (PBKDF2, zeroed-on-drop), mounts the root
  (`ARXFS::open`, wrong-passphrase fail-closed), and runs
  `load_users_db_source`. The FAT `root.unlock` reader that recovers the
  first of those three inputs is wired
  (`tairix_kernel::root_mount::read_root_unlock_descriptor`, `plans/PI.md`
  P11 Chunk B-1 — host-proven): it reads the fixed-length descriptor off the
  FAT boot partition through the same real FAT32 driver that authored it
  (one on-disk definition, §2.2; the shared `ROOT_UNLOCK_NAME` constant) and
  fails closed on a missing/truncated/over-long file before any read. The
  single boot-path entry that threads those two halves together is wired
  (`tairix_kernel::root_mount::mount_root_and_load_users`, `plans/PI.md` P11
  Chunk B-2 — host-proven): given the two brought-up block devices and the
  typed passphrase it reads the descriptor and, on success, runs the unlock
  composition, auditing and fail-closing a descriptor that cannot be read
  (`RootMountError::DescriptorRead`). The single-disk entry above it is wired
  (`tairix_kernel::root_mount::mount_root_disk_and_load_users` — host-proven):
  given **one** whole-disk block device it parses the partition table through
  the shared, scheme-neutral `lib/partition` layer (MBR encode + fail-closed
  MBR/GPT parse, the one on-disk definition `tools/mkimage` writes, §2.2 /
  §2.20 — works for a Pi MBR card and a UEFI x86_64 GPT disk on any arch),
  locates the FAT boot and `ARXFS` root partitions by role, opens a
  bounds-checked `PartitionBlock` window onto each in sequence (one device,
  two windows via `impl Block for &mut B`), and runs the composition —
  fail-closing a malformed/forged table or a missing partition. Root-device
  *discovery* is wired: the
  root-storage bind gate (`tairix_kernel::root_storage`, audited `4135`
  `ROOT_STORAGE_AUTOLOAD`) resolves which discovered hardware-tree node
  carries the bootstrap root block device against the in-kernel floor
  catalogue through the same shared `lib/devmatch` policy `devmgr` uses
  (§18.3 / §18.6) — read-only, fail-closed (no block device → unbound; >1 →
  ambiguous), so the metal boot is unaffected. A device behind a probed bus
  is enumerated too: the bootstrap-floor virtio-MMIO enumeration
  (`hwdiscovery::observe_virtio_mmio_block_devices`) reads each
  `virtio,mmio` slot's `DeviceID` and folds a probed `HwMatchKey::virtio(2)`
  child node into the same selection, so the QEMU `virt` boot binds its
  virtio-blk root (a no-op on the Pi, which has no `virtio,mmio` node, §2.17).
  The board storage bring-up that *supplies* the typed passphrase and brings
  the bound driver up is wired (`plans/PI.md` P11 Chunk B-2): the init seam
  admits the in-kernel root-unlock kthread
  (`tairix_kernel::unlock_service::spawn_if_present`), which brings the bound
  block driver up through an in-kernel block DriverHost behind the signed §8
  load gate, prompts on the primary console, and runs the interactive unlock
  policy — proven end to end on `-M virt` by the `root_unlock_login` (policy)
  and `root_unlock_admission` (full kthread-admission boot) verticals. It
  dispatches on the bound floor block driver (`run_unlock` →
  `virtio_blk_unlock` over the device-IRQ path, or `emmc2_unlock` — which
  brings the SD host up over its own bound GIC interrupt line, parking on
  completion rather than busy-spinning the SDHCI status register, §17.1/§2.16);
  the EMMC2 arm is wired and host-tested at the driver level, with its live
  SD-card mount metal-gated (`raspi4b` cannot model EMMC2, §0.4 /
  P8 / B4). The login
  `Run` binary ships at
  `/System/Services/login`
  (PID 1's `session` directive points at it): it obtains the kernel-held
  database through the `CAP_USERS_READ`-gated `users_db_read` syscall
  (`abi-v1` no. 19) and acts on its three-state result before each round
  via `tairix_login::supervise`: it **waits without prompting** while the
  read is `WouldBlock` (the encrypted root is still being unlocked, so it
  never draws `Username:` over the unlock kthread's `Root passphrase:`
  prompt on the shared console), wires `UsersAuthenticator` for a delivered
  database, and wires a deny-all authenticator once the read resolves with
  no database held (an installer image refuses every login, §5.4.5). On
  success it spawns the record's shell of choice via `spawn`/`wait`. The
  embedded-program registry carries per-program
  capability grants and argument vectors (`EmbeddedProgram` — login
  additionally holds `CAP_PROC_SPAWN` + `CAP_USERS_READ`; the shell only
  the console pair). Per-console sessions are wired: the kernel installs
  one stream backing per installed text console (`BootInfo::with_consoles`
  — the video console when a display is active, else the discovered UART;
  a UART beside an active display carries only the debug log and hosts no
  session), the
  per-process descriptor table records each descriptor's console index,
  `spawn`'s console selector (`CONSOLE_INHERIT` or an explicit validated
  index) plus the `console_count` syscall (no. 20) let PID 1 `init`
  supervise one login per console with wait-any reaping and per-console
  relaunch budgets. Terminal local echo is the kernel's read
  line-discipline (`stream_read` echoes consumed bytes, CR/LF→CR-LF),
  toggled per console by the `stream_echo` syscall (no. 21,
  `CAP_CONSOLE_READ`) that login uses to suppress echo around a password
  read. The video console's keyboard-input *delivery seam* is wired: a
  keyboard-input driver pushes decoded console bytes into a console's
  kernel-side `ConsoleInputQueue` through the `console_input` syscall
  (no. 22, `CAP_INPUT_INJECT`), which a video-login `stream_read` drains
  (with a display active the UART is the session-free debug log line). The
  keyboard *producer* is now wired host-side: the shared terminal key map
  `lib/keymap` (`encode_key` — `Key`+`Modifiers`→console tty bytes,
  allocation-free, reusing the `lib/vt` escape vocabulary, §2.2) plus the
  `lib/hid` `console` module (the US HID-usage→`Key` table
  with modifier + caps/num-lock state, and the `pump_once` driver loop
  that injects the bytes through a `ConsoleSink` — `console_input` on
  metal). Remaining console wiring (the Pi VL805/xHCI **metal** path that
  delivers the HID reports, and configurable log policy) is staged in
  `plans/PI.md` P11; login's
  authenticate path on a real volume additionally rides the production
  `mem_map` producer (`plans/SPAWN.md` SP5b, landed — so the `lib/rt`
  userland heap is live, `AGENTS.md` §25; login's path to its prompt
  nonetheless stays allocation-free by design).

### Stage 6 follow-up — Rust I/O abstraction (`plans/IO.md`)

**Status: done — the library (IO1–IO3), userland adoption (IO4), and the
one-descriptor-path unification (IO5) are landed. IO6 (descriptor-honest ABI
names) is planned.**

The ergonomic `std::io`-equivalent library lives in `lib/rt/src/io.rs` (module
`tairix_rt::io`): one fd-generic `Read`/`Write` trait pair with looping
`read_exact`/`write_all`/`write_fmt`, buffering (`BufReader` with
`read_until`/`read_line`/`lines`, `BufWriter` coalescing small writes over a
const-generic inline buffer), and the four well-known standard streams
(`Stdin`/`Stdout`/`Stderr`/`StdInfo`) plus a borrowed `Stream` over any
descriptor and the owning `File`. It is a pure layer over the existing `abi-v1`
`stream_read`/`stream_write` traps — **no** ABI surface, syscall, or capability
(`AGENTS.md` §5.4) — `no_std` + fail-closed (§2.9). The standard streams and any
file / resource-reference / tty / pipe fd a sibling plan later opens share this
one definition (§2.2, proved by a test exercising a `Stream` over a non-standard
fd through the identical trap path). `StdInfo` (fd 3) writes are best-effort
(§20.1); opening a *new* fd stays a capability-checked operation owned by
`plans/DRIVES.md` / `plans/ALIAS.md`, never invented here. `File` is the one
**owning** descriptor handle for every backing (path, resource reference, pipe
end, pty end — the close trap is backing-generic), so no second owning fd type
exists (§2.2). TAIRiX builds **no** system-wide C `stdio`
(§16.4, `plans/CCOMPAT.md`). **IO4 done:** the in-tree callers
(`userland/shell/elsh`, `userland/system/init`, `userland/apps/top`, and the
`sysinfo`/`ps`/`top` output path shared through `lib/procinfo`) write through
`tairix_rt::io::{Stdout, Stderr, Write}` and their hand-rolled short-write
loops are deleted (§2.14) — one `Write::write_all` loop in userland. The
bounded/edit-aware line readers (the REPL's `MAX_LINE` `LineReader` and
login's prompt reads, both over the shared `tairix_vt::line::LineEditor`)
are retained as a security bound (§24.4), not the unbounded
`BufReader`.

**IO5 done — one descriptor I/O path, files in the vocabulary, honest
failures.** The kernel carried the byte-movement path twice — `fs_read`/
`fs_write` (explicit offset) and a standard-stream-only `wired_stream_read`/
`wired_stream_write` (shared cursor) were near-verbatim copies that had already
drifted on the pipe read timeout. They are now one `descriptor_read`/
`descriptor_write` parameterised by a `StreamPos` (`At(offset)` positional,
`Cursor` sequential), with the caller-owned and delegated path arms sharing one
`read_path_backing` helper, so the direction gate, capability checks, and
copy-in/copy-out boundary exist once (§2.2). `stream_read`/`stream_write` no
longer stop at `STD_STREAM_COUNT`: they serve **any** descriptor the caller
holds, so a pipe end, pty end, resource, or file at fd ≥ 4 is finally readable
and writable sequentially; the console table stays the fallback for a standard
descriptor with no open entry, and no authority widens (the caller already held
the descriptor and could already reach it positionally). In userland the
primitives surface the kernel's `Errno` as `io::Error::Os` instead of collapsing
a refusal to a count of `0` — a fail-*open* read loop that silently truncated
(§2.24, §5.4) — so `Ok(0)` now means end-of-input and nothing else, `stdinfo`
excepted by its §20.1 contract. `File` implements the same `Read`/`Write` as
every other descriptor (sequential, at the shared description cursor) and its
positional `read_at`/`write_at` reuse the one `read_fill`/`write_drain` transfer
loop, so userland has one loop rather than three; the lossy
`tairix_rt::{stdin, stdout, stderr, stdinfo, stdin_timeout}` free functions are
deleted (§2.14) and every caller moved onto the traits.

**IO6 planned — descriptor-honest ABI names.** `fs_close` releases *any*
descriptor and `fs_read`/`fs_write` serve every backing, so the `fs_` prefix
reads as a filesystem gate that is not there; the rename to the `fd_*`/
positional-`stream_*` family (with the syscall-table hash and C header
regenerated) is its own landing because it touches ~40 files and the ABI
surface. Recorded, not silently deferred (§2.18).

See `plans/IO.md` (binding under `AGENTS.md`).

---

## Stage 7 — Graphics, Window Manager, Taskbar

**Dependencies:** Stage 6 + a display driver from Stage 4.

**Deliverables**
- `userland/gui/wm`: compositing window manager. Per-window surfaces, damage
  tracking, GPU acceleration where a driver exposes it, software fallback
  otherwise. The compositor must support:
  - **Rounded window corners**: per-window corner radius applied during
    composition (anti-aliased), with a square-corner setting retained for
    windows that opt out.
  - **Alpha transparency**: per-surface and per-region alpha so a window can
    be wholly or partially translucent; the compositor blends translucent
    surfaces against what is behind them with correct premultiplied-alpha
    compositing.
- `userland/gui/taskbar`: a traditional desktop taskbar (in the style of
  GNOME/Windows), pinned to a configured screen edge. Layout:
  - **Leading**: two permanent, fixed-order launcher buttons — **Library**
    (opens the folder-organised program-library popup fed from the merged
    `lib/proglib` catalog) and **Files** (opens the file manager,
    idempotently). Session controls (log out, lock, shut down, restart)
    belong to the Switchboard's System menu (`plans/NEW-TASKBAR.md` T13).
  - **Middle**: a task list showing currently running tasks (one entry per
    top-level window/application), with focus/activate and minimise/restore
    on click.
  - **Trailing**: a **notification icon area** for status/notification
    icons, then the clock, then — always trailing-most, reserved, immovable
    — the **Switchboard** tray capsule (`plans/NEW-TASKBAR.md` T9): live
    Normal / Job Active / Pressure / Hung / Recovery states from the
    monitor service's summary plus the session's own hang detection, with
    the hover/pinned instrument readout, scroll task-cycling, and
    middle-click previous-task switch.
  - **Rounded edges**: the taskbar itself supports rounded corners, drawn
    through the same compositor rounded-corner path as windows (no duplicate
    implementation, `AGENTS.md` §2.2).
- Theming: a **default dark theme** plus a **light theme**, switchable at
  runtime. Themes drive colours, corner radii, fonts, and cursors for the
  WM, taskbar, and default apps through one shared theme definition; adding a
  theme is data, not new code.
- Default cursor set (themed).
- **SVG-first graphical assets** (`AGENTS.md` §10). Every WM/desktop
  graphical asset — cursors, icons, notification glyphs, window-chrome
  artwork, theme decorations — is authored as **SVG** so one source stays
  crisp at any DPI / UI `Scale`. SVG is never parsed or drawn on the hot
  compositing path: an asset is rasterised/converted **once** at the active
  scale into the fast-draw form the compositor blits (a `lib/raster`
  `Surface`, or an intermediate vector form like `lib/cursor`'s) and that
  form is cached, re-rendered only on a scale or theme change — so the
  desktop stays quick. There is one rasterisation/blend path (`lib/raster`),
  never a second (§2.2); SVG decoding is untrusted input and runs through the
  curated §16.4 image-decoding shared library inside a §19.5 parser sandbox,
  failing closed to a fallback rather than crashing the compositor (§2.9).
- Default apps under `userland/apps/`:
  - **Filesystem browser**: navigates the §16 filesystem layout, honouring
    capability-gated permissions; no `/proc`/`/sys` fabrication (§16.1).
  - **Terminal emulator**: runs the default shell with job control.

**Tests**
- Headless compositor tests using a virtual framebuffer, including
  rounded-corner masking and per-region alpha blending (premultiplied-alpha
  correctness, fully-opaque and fully-transparent edge cases).
- Taskbar layout tests: the two permanent launchers at the leading end and
  the program-library popup they open, running-task list in the middle,
  notification area, clock, and the reserved Switchboard capsule at the
  trailing end (the capsule survives every narrower screen the clock and
  icons collapse on); rounded-edge rendering.
- Theme-switch tests: dark ↔ light applies consistently across WM, taskbar,
  and default apps.
- Input routing tests (focus, click-to-activate, drag-and-drop).

**Docs**
- `docs/src/desktop/{wm,taskbar,apps,theming}.md`.

**Status: in progress.**

Desktop paradigm: traditional GNOME/Windows-style `userland/gui/taskbar` (the
RISC OS iconbar idea was dropped; §3/§10 updated).

Full **icon-bar** build-out (staged, `in progress` — T1–T12 done) —
`plans/NEW-TASKBAR.md`: a first-class, folder-organised program library,
landed **as data** (T1–T3): `lib/proglib` (closed folder taxonomy, validated
entry model, `<id>.<field>` grammar, bounded fail-closed parser, canonical
render, the machine ∪ user-overlay merge with the overlay's visibility
verdict last, and the `reconcile` discovery fold), the
`userland/apps/applib` admin command (list/add/remove/hide/show/rescan, GNU
conventions, fd-3 records), the signed `AppInfo` `library` listing (opt-in
folder + icon; C header regenerated), and the image-build seeding
(`tools/mkimage` derives `/System/Settings/ProgramLibrary/library.conf` from
the planted bundles' own manifests) — and **as UI** (T4/T5): the two
permanent leading launchers (Library — accent `IconButton` with the new
`lib/icon` `Library` glyph, pressed-in while open; Files — quiet folder
glyph, resolved idempotently via the session's attested `LaunchTable`) and
the fully keyboard-navigable, searchable, `lib/controls`-composed
program-library popup (modal at both routers, fail-closed geometry, calm
empty states, session-loaded catalog re-read on every open), with the
generic start menu deleted — and **as pins** (T6/T7): the `lib/taskpins`
per-user ordered pin store (`~/Settings/Taskbar/pins.conf`, fuzzed), the
bar's pin strip + right-click context menu (shared `TaskbarItem`/`Menu`
controls; live Running/Active/Minimized/Closed state derived from the task
list; per-application icon artwork rasterised in the parser sandbox — the
new `lib/image` fail-closed PNG decoder, `lib/compress` inflate/zlib, and
the `lib/sandbox` icon-rasterisation service — with the class glyph as the
fail-closed fallback), the session's store ownership (one read + one write
seam, memory adopts an edit only after the write lands), and both
pin-creation gestures over the window channel's new `PinBundle`/`DragOffer`/
`DragWithdraw` ops (`BundleRef`-validated; files context action + the
`lib/browse` `BundleDrag` drag source; drops resolve through the shared
`resolve_pin_drop` policy at the strip's drop index) — and **as live
status** (T8–T10): the notification area (the fuzzed `notify_ipc` channel
over the seat-scoped `NOTIFY_ENDPOINT`, producer-attested relay,
click-to-dismiss popover), and the always-rightmost **Switchboard tray**:
the immovable capsule (shared `TraySignal` + count/alert badge; scroll
task-cycling, middle-click previous, hover readout), the seat-scoped
`SWITCHBOARD_ENDPOINT` + `switchboard_ipc` summary vocabulary, the
session's attested relay + `HangTracker` delivery-evidence hang detection,
and the `userland/gui/switchboard` monitor service (tickless
`lib/procinfo` sampler, change-only publisher, capability-sized manifest
intersected with the seat user's ceiling, spawned by the session and calm
on death) — and **as a live panel** (T11): the monitor hosts its own screen
composition (`userland/gui/switchboard::view`, assembled from the shared
controls) as a real window with genuinely working actions, implementing
`plans/desktop1.png`'s Open Panel. The
composition gained the shared `MetricTile` (its `Track` instrument is the
rail-tinted rounded track, and its honest `Unmeasured` state never fabricates
a zero) and its history counterpart `Chart`, `select_section`
(the host chooses the opening section) and `set_model` (an in-place data
refresh preserving section, per-section scroll, focus and any in-flight
drag; row-indexed selection/hover/armed presses are dropped so a press can
never complete against a replacing row). `switchboard_ipc` gained the two
owner-directed requests (`ActivateOwner`/`RestartOwner` — the session owns
the window stack, so the panel asks it), a publish reply attesting the
session's `ProcId`, and the per-instance `command_endpoint_for` command
mailbox (`OpenPanel`, and the `SeatReport` naming the owners the session's
delivery evidence proves unresponsive), every frame fuzzed and fail-closed.
The capsule's tap opens the task list and a 500 ms hold opens recovery (the
interim pin-on-press API is gone). Task/job/recovery actions are real:
`signal`'s target rule widened in place to **own child → same principal →
`CAP_PROC_CONTROL`** (id 40, administrative ceiling only), with
`ProcessSignal` split into `resolve_child` + `signal_task` over one delivery
engine and every cross-principal decision audited (event 4036, `Warn` on
refusal); an action whose authority the service lacks renders with the
Authority Mark and is never attempted — and **as the Pressure + Activities
panels** (T12): the "why is my machine slow" cause cards driven by the
tray's own latches (measured culprit, rail + heat seam, Pause / Lower
priority / Show tasks with truthful Ready/DisabledByState/DeniedByAuthority
verdicts re-checked at apply), the live session-lifetime activity groupings
(`proc_id`-keyed, single-membership, Group menu + inline rename,
Switch/Pause/Resume/Close sweeps over sample-joined members only), the
keyboard action-focus that made every row button reachable, and the new
scheduler surface behind "Lower priority": `SchedulerPolicy::set_priority`/
`priority` across all three policies + conformance, syscall 104
`sched_set_priority` (signal's target rule + a `CAP_PROC_CONTROL` raise
gate, audit event 4037), the `ProcessRecord.priority` field, and the
regenerated C headers (`plans/NEW-TASKBAR.md` T12) — and, completing the
stage (T13–T15), the **System quick-actions menu** (session controls, the
live Light/Dark toggle, System Settings; the screen lock re-authenticates
through the per-console elevation broker and the relayed power transitions
go through a trusted confirmation prompt), the T14 interaction-fidelity
pass, and T15's docs + **QEMU pin vertical**
(`tests/integration/taskbar_pin_qemu_aarch64`: boot the desktop, pin a
program-library entry through its context menu, open the Switchboard, and
launch the app from its new bar slot, with the bar read back out of the
scan-out). The panel's services list stays honestly empty pending a System
Information API service-enumeration query rather than fabricating rows.

Shipped (headless-testable, model + renderer over injected seams):
- `userland/gui/wm` software compositor: premultiplied-alpha blending
  (Porter–Duff `over`), `Surface`/`geometry`, anti-aliased rounded corners via
  supersampling (square opt-out — the one rounded-corner path, §2.2), damage
  tracking, window ops; fails closed on bad modes.
- Shared desktop libs (§2.2, one path each): `lib/raster`, `lib/theme`,
  `lib/geometry` (DPI/`Scale`),
  `lib/reclaim` (the reclaimable-cache model the desktop's cursor,
  notification-glyph, pinned-artwork, glyph and window-furniture caches are
  all built from, shared with the kernel's own caches; a process gauge admits
  nothing until told the band, so every `Run` binary that caches arms the
  pressure wake through the one `tairix_procinfo::pressure` helper and
  `lib/font` primes it when it builds its cache — `plans/SMARTRAM.md` SMART5,
  `plans/FONT-SERVICE.md` §3.2),
  `lib/font` (the text-rendering front end: the compiled-in console atlas
  — every face of the `mono` family, its Japanese, Korean and Hebrew
  companions included, `cargo xtask font-atlas`, drift gated in `ci`, with
  binary-search Unicode lookup and a U+FFFD fallback — that `lib/fbcon` draws
  verbatim, plus, behind the `render`
  feature, a thin cached `FONT_ENDPOINT` client of the `fontd` service that
  holds no font data of its own and lays proportional and fixed-pitch text
  out through one per-glyph-advance path; its `assets/` tree is the shipped
  `/System/Fonts` store, one directory per family),
  `lib/fontface` (the shared TrueType parser + anti-aliased rasteriser,
  grid fitter, variable-font instancing, and family resolution, used by both
  the `font-atlas` generator and the `fontd` service so the atlas and live
  text share one rasteriser, §2.2; its `store` module is the one `FontFamily`
  manifest parser the image builder and the service both read),
  `lib/cursor`, `lib/icon`, `lib/svg`, `lib/input`, `lib/procinfo`.
  - **Font-as-OS-service (done, `plans/FONT-SERVICE.md`).** Text rendering is
    a single sandboxed OS resource: the sandboxed `fontd` service
    (`userland/system/fontd`, `/System/Services/fontd.app`) is the only
    process that holds a font face or runs the TrueType rasteriser. It
    discovers the `/System/Fonts` store, rasterises in a §19.5
    minimum-capability sandbox, and serves 8-bit glyph coverage over the
    reserved `FONT_ENDPOINT` (`lib/abi/src/font_ipc.rs`). `lib/font`'s render
    path is a thin cached client with the ~10 MB of embedded atlas + TTF faces
    deleted, so no GUI `Run` image carries a font payload; the
    kernel/`lib/fbcon` boot console keeps only the small primary-face
    console-atlas subset (boot floor).
  - **Selectable font families, proportional desktop text.** The store is one
    directory per family (`FontFamily` manifest + its ordered faces), so
    shipping a family is dropping a directory into `lib/font/assets/` — no
    list anywhere names a face. Shipped: `inter` (the default proportional UI
    face the design boards are set in), `noto-sans`, `noto-serif`, the
    fixed-pitch `mono` (the console-atlas source), and the non-selectable
    `sans-fallback` carrying Hebrew + Chinese/Japanese/Korean coverage once
    for all three proportional families. The protocol is family-aware and
    every glyph reply carries its own advance and left bearing, so desktop
    chrome is genuinely proportional while the terminal keeps its grid. Faces
    are upstream **variable** fonts committed unmodified, instanced at a real
    `wght` by `lib/fontface` (`fvar`/`avar`/`gvar`+IUP/`HVAR`). A theme names
    its families as validated keys and `Fonts::with_ui_family` applies a
    user's choice; `FontRequest::Families` reports what the store holds so a
    settings surface offers exactly that. **Remaining:** the desktop-side
    picker — a system-menu row per family, persisted per user and validated
    against the reported list on session start.
  - **`fontd` starts with the desktop, not at boot** — text is a
    graphics-only resource,
    so `login` starts it (as its uid-15 account, via `CAP_SPAWN_AS_USER`) the
    first login round a machine is display-capable, covering both a graphical
    login and the shell `desktop` command and never a headless/text boot
    (§17.3); it resolves by path from the on-disk `/System/Services` bundle on
    aarch64 and from the compiled-in program registry on x86_64/riscv64.
    Post-boot start is the headless-first-correct design in its own right; an
    earlier concurrent-spawn crash worry (D18) was closed non-reproducing once
    the ~10 MB payload was removed (`plans/OPEN-DEFECTS.md`). The independent
    profile fix (`pie_build::cross_compile_pie_elf` reading `ImageProfile`)
    ships `installer` userland/drivers `--release`.
- `userland/gui/taskbar` (permanent Library/Files launchers + program-library
  popup + pin strip with its context menu + running-task list +
  notification area/clock + the reserved Switchboard tray capsule) and
  `userland/gui/session` glue (theme registry, taskbar model, catalog
  loading/merging, pin-store ownership + sandboxed icon pipeline, launch
  table, `DesktopShell` event loop / `TaskBridge`, the attested
  tray-summary relay + `HangTracker` hang detection, the owner-directed
  activate/restart requests served against the live window registry and
  launch table, and the command mailbox carrying `OpenPanel` + the change-only
  `SeatReport`), plus the `userland/gui/switchboard` monitor service feeding
  the capsule and hosting the overview window (one wait over its sample
  deadline, its command mailbox and — while open — its window events;
  capability-gated actions that fail closed and report a refusal without
  ending the session).
- Two default apps (filesystem browser, terminal emulator) — each a
  host-tested model + renderer plus a live store bundle served over the
  window channel (`plans/APPWIN.md` AW3/AW4; the AW4 `Stream` wait source).
  The terminal forwards the logged-in user's environment to the shell (so the
  prompt shows the real user, `tairix_terminal::spawned::shell_env`) and hosts
  the shell over a proper **pseudo-terminal** — a real tty line discipline
  (echo, cooked line editing, `ONLCR`, `Ctrl-C`/`Ctrl-Z` job control,
  queryable window size) so the shell runs as it does on the hardware console
  (`plans/PTY.md`). PTY0–PTY4 code is landed: the shared `lib/tty` discipline
  the console is rewritten onto, the kernel `Pty` object + shared
  `ForegroundOwnership`, the unprivileged `pty_create` ABI (no. 97) with its
  `OpenBacking::PtyMaster/PtySlave` backings + shared parked read/write loop +
  pty-slave `stream_input_mode`/`terminal_size`/`console_foreground`, and the
  terminal rewritten onto one pty. The PTY4 QEMU vertical extension
  (echo/`ONLCR`/`Ctrl-C` witnesses) is the one remaining item.
  The terminal is a first-class desktop program (`plans/GUI-TERMINAL.md`): it
  sizes its window from the face it actually draws with (80×25 plus the shared
  `WindowFrame` furniture, stepping the *text size* down rather than the grid
  on a display too small — no compile-time window size), carries a per-user
  profile at `~/Settings/Terminal/terminal.conf`, eight colour schemes
  including a user-authored one, a right-click menu whose every advertised
  shortcut is really honoured, an in-window settings sheet built from the
  shared Reactive Alloy controls, and a typed screen-effect pipeline
  (translucency, compositor backdrop blur, scan lines, fuzz, phosphor
  persistence, wobble) animated off a one-shot frame deadline rather than a
  poll loop.
- `kernel/ipc::PortRegistry` named-port registry composed into `KernelState`;
  `ipc_send`/`ipc_recv` resolve endpoints against it. User space resolves a
  published `PortName` to its endpoint through the unprivileged
  `port_resolve` syscall (`abi-v1` no. 75; `tairix_rt::port_resolve` /
  `tairix_sys_port_resolve`), fail-closed (length bound before the copy-in,
  grammar check before the registry, `NotFound` on a miss) and proven live
  by the aarch64 driver-spawn vertical (the fixture kernel publishes the
  reply endpoint's name; the spawned stub resolves it before replying).
  The `ipc_recv` handler gates every receive against the port's
  `required_recv_caps` (the `call_recv` pattern) before any message is
  observed — bind-time proof alone left the receive path fail-open (any
  task naming an endpoint id could drain it); regression-tested.
- **Desktop input is seat-routed, never a named IPC port.** A port's
  receive gate is capability-only and cannot express "only the live
  seat-lease holder may drain", so the planned `desktop.pointer`/
  `desktop.keyboard` ports were dropped in favour of the seat model:
  `pointer_inject` (78, `CAP_INPUT_INJECT`) / `pointer_read` (79,
  `CAP_INPUT_READ` + live-lease owner gate) mirror `key_inject`/
  `keyboard_read` exactly, over a per-seat bounded zeroing pointer channel
  (one generic ring shared with the keyboard channel; 256 pointer records,
  drop-oldest). An unowned seat consumes and discards pointer records (no
  text-mode consumer; the driver never learns who holds the seat).
  Wrappers in `lib/rt` (`pointer_inject`/`pointer_read`), stubs in
  `lib/abi-sys`, C headers regenerated. The desktop session consumes both
  streams through `SeatInputChannel` over the injected `SeatEventReader`
  seam (`userland/gui/session::seat`, replacing the deleted
  `IpcInputChannel`/`MessagePort` IPC framing); named ports remain for
  service rendezvous.
- **The pointer record is device-resolved but screen-independent.**
  `PointerInput` carries a relative displacement (`MovedBy { dx, dy }`) or
  a resolved button edge, never an absolute position: only the seat owner
  (the desktop session, which owns the compositor) knows the screen
  extent, so `DeviceInputSource` accumulates displacements — saturating,
  clamped to the screen `Rect` it is constructed with (empty screen
  refused), starting at the centre — and drivers need no display-geometry
  authority (the libinput/Wayland split). A scroll wheel is carried as a
  `Scrolled { dx, dy }` tick record, consumed by the desktop scrollbar
  (`PointerScrolled` in `lib/input`, routed to the root viewport under the
  pointer). `PointerInput::from_device_event`
  (`lib/abi`) is the one device→seat mapping (axis deltas + scroll ticks + the shared
  `evdev` `BTN_*` codes, hoisted with `AXIS_X`/`AXIS_Y` into
  `tairix_abi::driver::input` from the lib/hid + lib/virtio_input copies).
- **The seat's pointer channel is fed by the real virtio-input driver.**
  `drivers/input/virtio_kbd` pumps every decoded event through the shared
  pointer mapping first (→ `pointer_inject`), else the keyboard producer
  (→ `key_inject`); one instance serves whichever device its node is.
  The first delivery of each input kind emits a per-kind one-shot
  `INPUT_DELIVERED` witness (`kind=key`/`kind=pointer`, at most two
  records). The autoload QEMU vertical now attaches the virtio-mouse
  sibling, injects a key, waits for the `kind=key` witness line, injects
  `mouse_move` through the QEMU monitor, and PASSes only when **both**
  per-kind witnesses appear — proving key + pointer delivery end to end
  through the discovery → signed gate → spawn → inject path.

**User-memory copy path & per-task address spaces (staged).** The kernel
`copy_from_user`/`copy_to_user` boundary (§5.4) behind every deferred payload
transfer, landed in increments:
- A — `kernel/mem::uaccess` `copy_in`/`copy_out` (per-page translate +
  fail-closed USER/permission checks, W^X-aware; `UaccessError`).
- B — per-task `AddressSpaceRegistry` (`UserAddressSpace` trait, fail-closed
  register/withdraw/resolve) composed into `KernelState`.
- C — `with_caller_aspace` threads the registry into the syscall handler
  without giving `kernel/syscall` a `kernel/mem` dep (§17.4).
- D (.1–.4) — payload copy-in/out wired for `ipc`, `cap_delegate`, and
  `random_get` (CSPRNG output drawn in fixed chunks, copied via `copy_out`,
  staging zeroised; unseeded → `EntropyNotReady`, never weak bytes, §22).

- E — per-arch live `copy_from_user` page-fault fix-up (`tests/SECURITY.md`
  §5) — **DONE.** The uaccess byte move runs inside each MMU port's
  exported fault window (`tairix_arch_api::uaccess`: set-once idempotent
  `GuardedCopyFn` slot + `copy_user_span`, plain-copy default for
  host/wasm32, shared `conformance` checks; an in-place extension of the
  §17.2 MMU slice, no new slice). Per-port windowed copies — x86_64
  `rep movsb`, aarch64 `ldp`/`stp` pair loop, riscv64 alignment-safe
  doubleword loop (a misaligned access may trap on real silicon, so the
  window only ever absorbs page faults) — with fix-up labels that set the
  error return themselves; the trap handlers rewrite only the saved PC
  (riscv64 frame `sepc`, aarch64 frame ELR slot, x86_64 the `#PF` stub now
  passes the frame's RIP-slot pointer to its dispatcher) on a kernel-mode
  data fault whose PC is in-window. Armed at each port's vector-install
  chokepoint (`install_trap_vector` / `init_vectors`; x86_64 pairs it with
  the dedicated `#PF` install on the production boot, refusing the boot on
  a conflicting slot). Surfaces as `UaccessError::Faulted` → the same
  oracle-free `BadAddress`. Proven live by
  `tests/integration/uaccess_fault_qemu_{riscv64,aarch64,x86_64}` (real
  read- and write-side in-window faults, CPU keeps running) plus host
  unit tests (slot semantics, window containment, walk propagation).

**Remaining this stage:**
- **Non-blocking app launch — the desktop must not freeze while an app
  loads (`plans/FIX-DESKTOP.md`).** `SyscallNumber::SPAWN` today runs the
  whole app load (VFS read + signature/hash verify + eager address-space
  image build) synchronously on the *caller's* task, so the desktop
  compositor task cannot return to its present/input loop until the
  launch finishes — the reported freeze. The fix (staged DESK-1…DESK-7)
  makes launch asynchronous: `SPAWN` admits the child immediately and
  returns its PID; the child loads its own image on its first scheduled
  slice (the `pre_resume`/`work` seam), so the parent keeps running and
  load failures surface via the child's exit + audit. The same principle
  fixes the file picker's synchronous directory listing on the compositor
  thread. It then goes **strictly ahead of Linux**: the eager per-child
  image copy is replaced by a demand-paged, copy-on-write image backed by
  a per-boot **verified shared image cache** keyed on the signed content
  hash — read-only text/rodata is one physical frame set shared across
  every instance, writable pages are COW, cost tracks the working set not
  the whole binary, and no unverified byte is ever mapped (the page-cache
  sharing Linux has, plus verification it does not). Audit done; no
  implementation has landed.
- **The display-client present path (`plans/DISPLAY.md` D7) — the
  graphical session goes live.** The binding design is fixed there
  (zero-copy shm double-buffer frames, per-present kernel lease check,
  endpoint-directed region grants, park-on-seat-input). **D7a (the kernel
  surfaces) is done:** `WaitSourceKind::SeatInput` (wake on input *and* on
  lease loss, owner-checked add, oracle-free refusals),
  `shm_grant` (`abi-v1` 82, `CAP_SHM`, audited, endpoint-directed
  recipient), and `call_peer_seat` (`abi-v1` 83, the `call_peer_origin`
  trust window, live-lease generation answer) — kernel host tests, `lib/rt`
  wrappers, `tairix_sys_*` stubs, C headers, and the syscalls/seat docs all
  landed together. **D7b (the display service) is done:** the
  fixed-width, fuzzed `lib/abi::display_ipc` protocol (`Query`/`Configure`/
  `Present`, the reserved squat-protected `DISPLAY_ENDPOINT`, the shared
  `lib/abi::reply` status frame), the `lib/display` crate (the
  `DisplayServer` engine — decode → `call_peer_seat` lease gate on every
  request, lease-generation-bound configure state, bounded present — the
  `DisplayClient`/`RemoteDisplay` halves with per-frame stale-damage
  double-buffer bookkeeping, and the hoisted linear-surface engine
  `Framebuffer`/`FramebufferConfig` the three framebuffer QEMU verticals
  drive as non-driver consumers), the in-place `Display::present_region`
  evolution (full-blit default; the WM compositor threads its damage
  bounds through it), the
  distinct `Errno::DeviceFault` (`DriverError::as_errno` maps
  `DeviceFault`/`Busy` to `DeviceFault`/`WouldBlock`), the
  `HwResourceKind::Framebuffer` scan-out resource (geometry-carrying
  window: validated `framebuffer_mode` decode, `sole_framebuffer` grant
  resolver, `mmio_map` admission) so a user-space display driver learns
  its surface from discovery, never a board constant, the in-place
  `shm_map` evolution (a `len_out` user pointer reports the kernel
  registry's own record of the mapped region's byte length, so a server
  — and the four shm-consuming driver programs, which verify it before
  building their slices — never sizes shared bytes from a peer's claim),
  and the framebuffer service `Run` binary itself
  (`drivers/display/framebuffer`, bin-only on the `virtio_kbd` shape:
  grants → `sole_framebuffer` → surface; `DISPLAY_ENDPOINT` bind under
  `CAP_IPC_BIND_PRIVILEGED`; `RtSeatCheck`/`RtShmMapper` seams; a
  waitset-parked serve loop with fail-loud reserved exit codes — its
  image bundle + bind keys ride the D7d autoload world). **D7c (the
  desktop session binary) is done:** `userland/gui/session` ships its
  `Run` program (freestanding lib+bin shape, `AppInfo.toml` requesting
  exactly `CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM`) — `display_acquire` →
  `DisplayClient` bring-up (query → checked frame arithmetic →
  `shm_create` double buffer → `shm_grant` to the endpoint's serving
  task → configure) → `RemoteDisplay` over its own mapping → the live
  `SeatEventReader`s over `tairix_rt::pointer_read`/`keyboard_read`
  drained after each `SeatInput` wake → `DesktopShell` pump → composite →
  present with damage; seat loss tears the session down fail-loud
  (stderr reason, reserved exit codes, owner-checked release on every
  exit path). **D7d's first stage is done:** the autoload QEMU vertical
  boots a display world — the aarch64 boot publishes the ramfb scan-out
  surface as a boot display node (`HwResourceKind::Framebuffer` grant +
  `simple-framebuffer` match key), the signed framebuffer-service bundle
  autoloads onto its grants, the whole unlock dialogue is typed at the
  seat keyboard (the video console is the only console), and the run
  proves both per-kind `INPUT_DELIVERED` witnesses, the unlock, and the
  `DISPLAY_ENDPOINT` bind (`plans/DISPLAY.md` D7d). **D7d-2 (the desktop
  launch) is done — D7 is complete:** the desktop is the `desktop`
  application in the system application store (`desktop.app`, the
  `userland/gui/session` `Run` binary) — typed as a bare command word at
  the shell, and spawned directly by login when `os.loginType` is
  `graphical` (`lib/sysconfig`, the default) *and* the per-round probe
  holds (a
  read-only `fs_open` of the bundle's `Run` path — login's manifest
  carries `CAP_FS_ACCESS` for exactly this — plus one `Query` `ipc_call`
  to the reserved `DISPLAY_ENDPOINT`); there is no per-login session
  selector, and a configured graphical default degrades to text when the
  probe fails. `SESSION_BASELINE` carries the graphical class
  (`CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM`, the CU6 ceiling slice) while
  the shell's manifest was decoupled to its own exercised set
  (`SHELL_MANIFEST`); the display service emits a one-shot
  `FIRST_PRESENT` record (id 15001, `CAP_LOG_EMIT`) after the first
  client frame reaches scan-out; and the autoload vertical types
  `root`/`root` + `desktop` at the seat keyboard, keys a QEMU screendump
  plus the mouse injection on that witness (present → verified dump →
  pointer → PASS), and the runner asserts the dump is dominated by the
  theme's desktop colour — the host-side proof the composited frame
  reached the surface.
  The virtio pointer feed into the seat channel is done — see above; the
  USB HID mouse joins through the same shared `from_device_event` mapping
  when its metal report pump lands. **The theme-switch relay is done:** the
  seat-input toggle resolves in the session registry, `DesktopShell::handle`
  re-colours the compositor's desktop in the same frame as the re-themed bar
  (`sync_background` over the runtime `Compositor::set_background` —
  opaque-forced, equal-colour no-op, full-screen damage), and the session
  loop's per-wake damage present carries the repaint over the display IPC;
  the one theme→render conversion (`From<Rgba> for Color`) also seeds the
  compositor at bring-up. Host-verified through the exact shell/compositor
  code the `Run` binary drives (wm + session tests), and end to end in the
  autoload QEMU vertical: the runner's ordered pointer-button script clicks
  the live toggle and a third verified screendump asserts the re-themed
  scan-out (`plans/APPWIN.md` AW3). Then: wire the two default apps
  to live VFS/shell channels + WM-presented windows — staged in
  `plans/APPWIN.md` (binding). **AW1 is done:** the shared
  `tairix_abi::fs::DirEntries` stream walker (unit-tested, fuzzed in
  `fuzz_decode`), the one `lib/rt` directory-listing call
  (`read_all_growing` + `read_dir_all`, host-tested; `ls` refactored onto
  it), and the files app's production `VfsDirectorySource` engine
  (validated path spelling — one spelling shared with `Browser::path` —
  plus stream→`Entry` mapping, host-proven end to end over encoded
  streams). **AW2 is done:** the fixed-width, fuzzed
  `tairix_abi::window_ipc` vocabulary (requests + create reply +
  app-ward events, `WINDOW_ENDPOINT` reserved in
  `is_reserved_endpoint`) and the `lib/window` engine crate hosting both
  halves — the `WindowServer` the session composes (caller attested via
  the `call_peer_origin` seam, windows keyed to the owner's `ProcId`,
  map-once shm regions, per-client cap, fail-closed teardown, validated
  event routing) and the `WindowClient`/`WindowEvents` an app links
  (typed calls + parked event wait) — host-proven through an in-process
  loopback. **AW3 is done:** the session serves `WINDOW_ENDPOINT` (bind
  authorised by its live seat lease) from its token-dispatched wait-set
  loop into `DesktopShell`; the files bundle (`CAP_FS_ACCESS` only) lives
  in the system application store and is spawned from the taskbar's
  permanent Files button; the autoload
  QEMU vertical click-drives the whole chain (Files button → launch →
  served-window clicks) with two verified screendumps, gated on
  kernel-attested serial records and the interaction contract in the test
  crate's lib target. **AW4 is done:** `WaitSourceKind::Stream` (the
  caller's own pipe read end, owner-checked at add, ready on bytes or
  end-of-stream over the pipe layer's existing wakes), the terminal's
  host-tested pipe/spawn `ShellSource` (`tairix_terminal::spawned`), the
  `terminal.app` store bundle (`CAP_CONSOLE_WRITE`+`CAP_PROC_SPAWN`+
  `CAP_SHM`) spawned from the program-library popup's terminal entry
  (`plans/NEW-TASKBAR.md` T5), and the
  autoload vertical's typed-command tail (guest PASS latches a
  `ProcessSpawned` at/after the Enter press's delivery — the shell round
  trip, kernel-attested). **AW5 is done (code + host coverage):** the
  kernel's one-shot read-only file delegation (`fd_grant`/`fd_redeem`,
  in-place `abi-v1` additions: recipient-owner-bound handles, grantor uid +
  effective set captured and re-checked by the secured VFS on every
  delegated read, audited, one-shot-atomic, reclaimed with the recipient),
  the window channel's `PickFile` request + `FilePicked`/`PickCancelled`
  conclusions (engine-enforced one conclusion per accepted pick), the
  browser engine hoisted to `lib/browse` (its second consumer), the
  session's trusted picker (`SessionPicker`, one slot, session-authority
  listings, key+click navigation over the shared hit-test; the session
  manifest gained `CAP_FS_ACCESS`), and the `viewer.app` consumer holding
  **no** filesystem capability — it reads exactly the one user-chosen file
  through the redeemed delegation. Remaining (staged in `plans/APPWIN.md`
  AW5): the autoload QEMU vertical's picker stage, which shifts the
  AW3/AW4 interaction contract's delivery counts and reply indices.
- **First-class file manager (`plans/NEW-FILEMANAGER.md`, in progress).** The
  Stage-7 `files.app` browser is staged into a full graphical file manager
  — clickable file/folder icons, open/launch `.app` bundles, hand files to
  viewers over the AW5 CU6 delegation, in-place rename, move/copy/delete,
  make-directory, and properties — built entirely on the shared
  `lib/browse` engine + `lib/controls` widgets + `lib/icon` (no second
  rendering path, §2.2), needing no new capability for the user's own
  §5.3-checked writes. Gated stages FM1–FM9; every stage host-proven
  in `lib/browse` and, where end-to-end, on the autoload QEMU vertical.
  **FM1** (richer entries — `size`/`modified`, the `Bundle` kind, the shared
  sort) and **FM2a** (the list item view: each entry a shared
  `lib/controls::TableRow` with name/size/modified columns over the one pure
  `lib/browse::layout::ListView` visible-window/row-rect/hit-test geometry —
  built on the shared `scroll::ScrollRange` clamp — plus the `format`
  size/date column formatting), **FM2b** (the icon-grid view, the runtime view
  toggle, and the drawn `ScrollBar`), **FM3** (the file-type icon classifier +
  drawn grid-tile glyphs), and **FM4a** (the engine navigation model — bounded
  back/forward history plus the `go_up`/`navigate_to` climb, all transactional
  and fail-closed) are **done**, as are **FM5** (in-place rename — the first
  write), **FM6a** (the activation decision), **FM6b's** and
  **FM7a's**/**FM7b's** pure engine models, **FM4b's pure chrome model** plus
  its **drawn clickable toolbar** (`Alt+←/→/↑`/`F5`), **FM8a** (the pure
  properties view model), and **FM8b's drawn read-only properties panel**
  (`render::draw_properties` — a shared `lib/controls::Panel` painting the
  `properties_rows` fields, opened by `Alt+Enter` / dismissed by `Escape`,
  reading its metadata with one capability-checked `fs_stat` under the user's
  own identity) and its **drawn permission (mode) control** (nine clickable
  owner/group/other `rwx` toggles overlaid inline on the panel's permissions
  row via `render::draw_properties_editable`/`permission_cell_at`, committed
  through `Browser::set_mode_selected` over `fs_set_mode` under the user's own
  identity — the read-only picker never draws or resolves a toggle).
  The manager's chrome carries the location in the **window title** rather than
  a path bar: it opens on the icon grid, retitles over the window channel's
  `SetTitle` whenever it moves, distinguishes an empty folder from a full one by
  a bounded per-visible-row occupancy probe, and treats a *secondary* press on
  the window's Close control as "leave this folder" — climbing to the parent and
  closing only at the top (a parent it cannot list keeps the window open and
  states the refusal).
  FM2/FM4/FM6/FM7/FM8 were each split (§2.19). FM4b's **drawn context menu**,
  all of FM7b's **app-side move/copy/paste/delete/new-folder verbs** (with
  interleaved progress + cancel), FM8b's **ownership editing**, and all of
  **FM6b** are now done: `OpenFile` resolves the associated viewer from the
  installed bundles' signed `AppInfo` MIME associations (the composer now emits
  that MIME table; the `files.app` `RtBundleSource` reads it via the shared
  `association_from_appinfo`) and hands the file over race-free at spawn —
  `fs_open` read-only + `spawn_attached` wiring the descriptor onto the child's
  `STDIN` with the reserved `tairix_abi::DOCUMENT_ROLE_ARG` token, so the
  viewer reads its document with no filesystem capability of its own. FM6b's
  explicit **"Open With…" chooser** is now done too: `OpenWith` re-joins the
  context menu for a regular file, and `render::build_open_with_menu` /
  `open_with_index_at` draw the full `applications_for` candidate list and
  launch the picked bundle through the same `DOCUMENT_ROLE_ARG`+`STDIN`
  hand-off (the default open picks the first association; the chooser lets the
  user pick any). **FM6b is complete, and FM9's first vertical increment
  FM9-a is landed**: the aarch64 `autoload_input` QEMU vertical now appends,
  after the AW4 terminal round trip, a New-Folder + inline-rename click-through
  that descends into `/Users/root` by layout-reconstructed pointer clicks
  (`render::selection_rect` for rows, the new forward `render::manager_tool_rect`
  over the new `Toolbar::tool_rect` for the New Folder tool, offset by the WM's
  `WindowFrame::insets` client inset) and seat-keyboard `Enter`s, creating and
  naming a folder; the guest PASS gate latches two new `FsNodeMutated`
  `op=mkdir`→`op=rename` witnesses (counted only after the terminal round trip,
  fail-closed) plus a "named folder" screendump. **FM9-b is landed too**: the
  trusted picker now opens at the user's home (`Browser::open_at` over the
  session's `HOME`, falling back to `/`), the shared users-root fixture plants
  a readable document in `/Users/root`, and the vertical launches the Viewer
  from the desktop's launcher, lets its auto-opened picker read the home, and clicks
  the document row — the session `fd_grant`s the chosen file to the Viewer and
  the Viewer `fd_redeem`s it (two new guest witnesses `sc=fd_grant`→
  `sc=fd_redeem`, after the FM9-a rename). The pick-click is gated on a
  test-kernel picker-open marker (the session's first post-rename
  `comm=desktop sc=fs_open`, the picker's home read) since the session-internal
  picker delivers no `MessageDelivered` and the user-authority session cannot
  `log_emit`; non-flaky across repeated runs. **FM9-c (delete with confirm) is
  fully landed**, completing FM9 and the whole `plans/NEW-FILEMANAGER.md` plan.
  A clickable **Delete** now joins the context menu
  (`ContextCommand::Delete`, its `begin_delete` action already existed, §2.4),
  routed to the same confirm-and-remove verb the `Delete` key drives. Reaching
  it fixed a real defect that makes the *whole* context menu usable in the
  desktop: secondary (right) button presses were dropped — `tairix_wm`'s input
  router ignored them and the session router had a catch-all that swallowed
  them. Now the WM router returns `InputResponse::SecondaryActivated` for a
  client right-press and the session forwards it and delivers `WindowEvent`
  `Pointer{Pressed(Secondary)}` to the app (host-tested in `tairix-wm` and
  `tairix-desktop-session`; shared `Menu::row_rect` /
  `render::context_menu_command_rect` locate the Delete row). The earlier
  "the injected right-click never arrives in QEMU" was a `tools/qemu` **harness
  bug**, not an emulator limit: QEMU's HMP `mouse_button` help string
  ("1=L, 2=M, 4=R") is wrong — `hmp_mouse_button` maps state bit `0x2` to the
  right button and `0x4` to the middle, so the harness (trusting the help
  string) sent a right-click as bit `0x4`, which QEMU delivered as a middle
  button. `MouseButton::mask_bit` now sends `0x2`, and the dedicated
  `tairix-test-pointer-button-virtio-mmio-qemu-aarch64` vertical proves a real
  right-click reaches the guest as `BTN_RIGHT` `0x111` (it times out with the
  old mask, passes with the fix — the fails-before/passes-after guard). The full
  right-click→Delete→confirm click-through is now wired into the aarch64
  `autoload_input` vertical: appended after FM9-b and gated on the Viewer's
  `sc=fd_redeem` (the last FM9-b serial event), the runner right-clicks the
  FM9-a folder row to open the context menu, clicks the drawn **Delete** row,
  and clicks the confirmation dialog's Delete button — every point reconstructed
  through the shared `render::selection_rect` / `context_menu_command_rect` /
  `delete_dialog_rect` + `Dialog::action_rects` geometry (§2.2). A tenth guest
  PASS witness latches from `FsNodeMutated op=rmdir`, gated after the FM9-b
  delegation is redeemed so no earlier removal can satisfy it (fail closed);
  non-flaky across repeated runs.
- The platform-RNG `EntropySource` that seeds the reserve — **DONE**
  (`.junie/PREREQUISITES.md` P-0): the Arch-HAL `tairix_arch_api::entropy`
  slice (x86_64 `RDSEED`/`RDRAND`, aarch64 `RNDR` `Supported`; riscv64 `Zkr` /
  wasm32 host-import honest `Pending`) seeds the kernel reserve at boot via
  `KernelArch::platform_entropy`.
  - **Open gap — Pi 4 metal boots unseeded.** The aarch64 source is
    `FEAT_RNG`/`RNDR` only, and the Pi 4's Cortex-A72 (ARMv8.0) has no
    `FEAT_RNG`: on metal the seed draw fails closed (`id=4061
    entropy reserve unseeded cause=draw_failed`, then `id=4063 per-boot id
    unavailable`) and `random_get` keeps returning `EntropyNotReady` — honest,
    never weakened, but the flagship board has no cryptographic randomness.
    Staged work: a BCM2711 RNG200 (`brcm,bcm2711-rng200`) entropy source —
    discovered from the device tree, MMIO-mapped by the aarch64 port, health-
    tested, and mixed through the same `MixedPair` seam (never trusted alone,
    §22) — so the Pi seeds at boot like the QEMU/x86_64 paths.
  The seed is never the hardware RNG alone
  (§22): it is XOR-mixed with two independent software sources — a CPU
  timing-jitter source and the asynchronous interrupt-arrival-timing pool
  (`lib/rng::interrupt`, fed wait-free from `IrqTable::fire` via a set-once
  observer and folded into every reseed) — so no single source is trusted
  alone. The encrypted-swap key (Stage 8) consumes the same seam.

### GUI controls — Reactive Alloy (`plans/GUI-CONTROLS-DESIGN.md`, binding)

The shared GUI control design language. Built foundation-first as
`lib/controls`, so the window manager, taskbar, and apps share one typed,
theme-resolved, single-drawing-path control implementation rather than
per-app recipes (§2.2).

- **Scroll geometry engine — DONE.** `lib/controls::scroll` is the one
  orientation-independent scrollbar behaviour the spec requires be shared by
  the WM root viewport and nested application content (§11.28): `ScrollRange`
  (validated, always-normalised content/viewport/offset, private fields so the
  offset can never exceed `max_offset`), `ScrollModel` (the single offset
  source of truth: line/page/`scroll_by`/`scroll_to`/`to_start`/`to_end`/
  `resize`), and `ScrollGeometry` (proportional thumb length bounded by the
  theme minimum and the track, offset↔thumb-position mapping, `hit`
  classification, and drag with a preserved pointer-to-thumb anchor). Pure
  `u128` integer arithmetic, every division guarded, fail-closed to a
  non-draggable zero-offset bar; 27 host unit tests cover the §20 scrollbar
  checklist.
- **Scroll engine consumers — DONE.** The engine has its two mandated,
  independent consumers, and the deferred scroll-tick record is now carried
  end to end. The window manager composes **root-viewport scrollbars** as
  furniture (`userland/gui/wm::viewport::RootViewport`): a per-axis
  `ScrollModel`, a reserved-gutter/overlay layout, a furniture hit map that
  keeps the client from receiving bar input (and clips the client out of the
  gutter), and `InputRouter` wheel/track-page/thumb-drag driving over the
  Stage-A math. The **viewer app** is the second, nested consumer
  (`tairix_viewer::ScrollView`), scrolling a long file by keyboard through
  the same `ScrollModel`. The pointer record carries a `Scrolled` tick
  (`lib/abi`), delivered as `lib/input::PointerScrolled` and a new theme
  `scrollbar_breadth`/`min_thumb_length` metric sizes the furniture.
- **Server-side window decorations — DONE** (`plans/COMPOSITOR-WORK.md`). The
  window manager composes the `lib/controls::window` furniture family
  (`WindowFrame`/`TitleBar`/the four command controls/`ResizeGrabber`) around
  every served application window: the compositor reserves the frame band,
  renders the chrome, keeps a furniture hit map (a frame press is never a
  client press), and routes pointer/keyboard to typed control actions. The
  desktop session turns it on — `ShellWindowHost::window_opened` decorates each
  served window via `DesktopShell::decorate_window` (always movable, titled from
  the channel `WindowTitle`, and resizable when the app's create asks for it),
  `sync_active_frame` keeps exactly one active frame following focus, and each
  command control maps through the one shared `window_control_event` to the
  window lifecycle over the existing window path (Close→`CloseRequested`,
  Minimize→hide+`Minimized`, PutToBack→restack, SizeToggle→`Resized`) — no new
  syscall, no ambient authority. Client-driven resizability is live: the create
  request carries a `resizable` flag, a resizable window gets the grabber + live
  size toggle, and the file viewer (`userland/apps/viewer`) and the terminal
  re-lay-out on `WindowEvent::Resized` (re-mapping their region via
  `WindowRequest::Resize`); Files presents fixed size. A window may also ask
  the compositor to frost what is behind it (`WindowRequest::SetBackdropBlur`,
  a separable O(area) box blur under the window's own rounded-corner
  coverage, with damage widened to the whole frosted window so a change behind
  it cannot leave stale pixels, and the accelerated layer path falling back to
  the software composite for such a frame). The trusted file picker stays
  undecorated session chrome. No app draws its own chrome.
- **Typed control-state vocabulary — DONE.** `lib/controls::state` is the §5
  model as composed typed Rust: `ControlKind`/`ControlRole`, a `ControlState`
  built from `FocusState`/`PointerState`/`SelectionState`/`ValidationState`/
  `AuthorityState`/`ActivityState`/`PressureState`/`RecoveryState`, the derived
  §13 `ControlDisposition`, `ProgressValue`, and the window-furniture states
  (`WindowControlKind`/`WindowActivationState`/`WindowSizeState`/
  `WindowFurnitureState`). Composition over one giant enum; illegal states
  unrepresentable.
- **Theme-token additions — DONE.** `lib/theme` carries the §6 Reactive Alloy
  tokens as data: semantic signal roles + `Palette::signal`, the control /
  furniture metrics (seam/rail/bead/thumb/min-thumb/extents), `MotionTheme`
  with reduced-motion, and `Density`/`Contrast`; both built-ins populate them.
- **Shared rounded-rectangle fill — DONE.** `lib/raster::round_rect_coverage`
  is the single anti-aliased rounded-rect coverage definition (supersampled,
  no `sqrt`, fail-closed radius clamp); `Surface::fill_round_rect` fills
  through it, and the WM compositor's per-window corner rounding
  (`userland/gui/wm::corner`) consumes the same function, so window corners
  and control plates can never diverge (§2.2).
- **Drawn controls: button family — DONE.** `lib/controls::button` draws
  `Button`/`IconButton`/`SplitButton` from one visual+interaction core over
  `lib/raster`/`lib/icon`/`lib/font`: Alloy Plate + Signal Rim through the
  shared rounded-rect, Heat Seam / Pressure Rail / shape-coded Signal Bead
  (check/diamond/lock) and focus ring, full pointer/keyboard activation, and
  the §13 disabled-vs-denied-vs-pending-vs-failed rendering, all theme- and
  `Scale`-resolved. Dark/light + high-contrast + accessibility (shape marks)
  tested.
- **Drawn controls: boolean-selector family — DONE.** `lib/controls::selector`
  draws `Toggle`/`Checkbox`/`Radio` over the shared draw+interaction core
  (`lib/controls::paint`, hoisted from the button family so the §13 rim/bead
  recipe and plate rounding live once, §2.2). Each reads by shape as well as
  colour (toggle thumb + accent contact, checkbox filled-square / mixed-bar,
  radio centre bead), draws the shared overlay signals (Pressure Rail, pending
  Heat Seam, shape-coded Signal Bead) after the glyph, carries the §13
  disposition on rim + mark + Authority Mark (a denied selector keeps its value
  and shows the lock bead), and emits a typed `SelectorAction::Set { on }`.
  Dark/light + high-contrast + accessibility (shape marks) + pointer/keyboard +
  next-value semantics tested. The selector mark-colour recipe is hoisted into
  the shared `lib/controls::paint::resolve_mark` so the selector tick and the
  slider value track resolve their accent from one definition (§2.2).
- **Drawn controls: value-control family — DONE.** `lib/controls::value` draws
  `Slider` and `Progress` over the shared paint core. `Slider` is a measured
  control (groove, accent value track filling to a draggable thumb plate with
  rim + focus ring): drag updates the displayed value immediately and commits
  through the owner via `SliderAction::SetValue`, arrows/Page/Home/End step it
  by settable line/page steps (zero step moves nothing, fail-closed bounds), a
  resource slider tints the track with its semantic signal colour, `with_cap`
  bounds the value with a warning cap marker, and the §13 disposition drives
  the rim/track and lock bead (a denied slider keeps its value, ignores input).
  `Progress` is a read-only instrument trace driven only by its `ActivityState`
  (and, for indeterminate work, an owner-advanced `phase` — no idle loop):
  known % fill + caption, working/indeterminate moving segment that freezes
  under reduced motion, complete success fill + check bead, failed recovery rim
  + reason. Value is a validated permille, all arithmetic `u64`-guarded and
  clamped. Dark/light + high-contrast + reduced-motion + scale + §13 tested.
- **Drawn controls: text-entry family — DONE.** `lib/controls::text` draws
  `TextField` and `SearchField` over the shared paint core and one pure
  `TextEditor` (caret/anchor byte indices always on a `char` boundary,
  insert/backspace/forward-delete, left/right/home/end with Shift-selection,
  Ctrl+A, optional character limit). Text is clipped and horizontally scrolled
  through a temporary sub-surface (caret pinned visible, no bleed over the rim),
  with an accent selection highlight and a focused/actionable caret; a pointer
  press places the caret and a drag extends the selection. The §13 disposition
  keeps read-only (recessed plate, full-contrast selectable text, no edits)
  distinct from disabled (muted) and denied (kept value + lock bead); validation
  drives the rim segment (Invalid/Warning) and an inline message row when the
  bounds allow. `SearchField` adds a leading magnifier that reads accent on a
  query and clears it on Escape. Both emit a typed `TextAction`
  (`Edited`/`Submitted`/`Cancelled`); the owner validates and commits. 30 host
  tests (editing incl. multibyte, caret/selection, limit, Ctrl+A, Enter/Escape,
  read-only/denied/disabled, validation rim + message, pointer caret/drag,
  dark/light + high-contrast + scale, search chrome).
- **Drawn controls: command-surface family + ComboBox — DONE.** The shared
  chevron and focus-ring/cell-outline primitives are hoisted into
  `lib/controls::paint` (`ChevronDir`/`paint_chevron`, `draw_outline`), then:
  `lib/controls::menu` draws `MenuItem` rows and the elevated `Menu` plate
  (icon column, shortcut/reason, submenu chevron, danger rail, §13 bead, current
  highlight vs keyboard focus ring; Up/Down/Home/End/Right/Enter/Space/Escape,
  pointer hover/click, `row_at`, `preferred_width/height`, typed `MenuAction`);
  `lib/controls::toolbar` composes `IconButton`/`SplitButton` tools in `u16`
  groups (raised strip, group dividers, active-tool accent seam, per-tool heat
  from button state; pointer routing + Left/Right/Home/End focus + activation,
  typed `ToolbarAction`); `lib/controls::tabs` draws an equal-width `Tabs` strip
  (selected lower seam, loading heat seam, modified/error shape beads, focus
  ring; Left/Right/Home/End + Enter/Space, typed `TabsAction`); and
  `lib/controls::combo` draws `ComboBox` composing the `Menu` for its popup
  (collapsed field + down-chevron, open/select/close by pointer and keyboard,
  outside/Escape dismiss, §13 denied lock bead, `popup_size`, typed
  `ComboAction`). All theme/`Scale`-resolved, dark/light + high-contrast +
  fail-closed; 63 host tests across the four families.
- **Drawn controls: collection family — DONE.** `lib/controls::collection`
  draws `ListRow`, `TableRow`, `TableCell`, `Card`, and `Panel` over the shared
  paint core and one shared row-chrome helper (`paint_row`). A row reads state
  by shape as well as colour: hover tint, a leading accent selection rail +
  raised tint, a leading semantic Pressure Rail, a bottom proportional activity
  Heat Seam, a trailing recovery/complete/denied Signal Bead, and a focus ring,
  with the §13 disposition driving the foreground (denied keeps its value + lock
  bead). The leading rail gutter is *always reserved* so content never shifts
  when a row's state changes — tables stay aligned (spec §11.13); `paint_row`
  returns the state-independent content rect. `TableCell` draws column-aligned
  text (leading/centre/`numeric` trailing) and its own bead only for
  cell-specific state (§11.14). `Card` carries dominant state (leading rail),
  progress (bottom seam), and a count pill / alert bead (top-trailing) with
  composed `Button` footer actions; `Panel` splits a header (title + state bead
  + right-aligned grouped `Button` actions) from an owner-drawn `content_rect`
  with an anchor notch (`anchor_edge`) back to its invoker. Typed `RowAction`/
  `CardAction`/`PanelAction`; authority stays with the owner. 32 host tests
  (both themes, high contrast, scale, rails/seam/beads, focus,
  pointer/keyboard, fail-closed, the column-alignment invariant, card
  three-edge state + footer, panel layout/actions/notch).
- **Drawn controls: scrollbar renderer — DONE.** `lib/controls::scrollbar`
  draws the one orientation-parameterized `ScrollBar` (spec §11.28–§11.30) over
  the Stage-A `scroll::ScrollGeometry`: a decrement button, a track
  (before-thumb/thumb/after-thumb), and an increment button, laid out
  identically on both axes so the vertical and horizontal bars are one
  behaviour, not two recipes. It paints the quiet Scroll Channel, a rounded
  thumb that brightens to the reactive rim when awake (hover/focus/drag/held),
  orientation-appropriate end-button chevrons (`paint::ChevronDir` gained
  `Up`/`Left`) that brighten under the pointer or when held, a focus-ring
  outline distinct from a hover, a high-contrast thumb rim, and the §13
  disposition (denied shows the denied thumb and ignores input, disabled
  mutes it). It holds the owning viewport's `ScrollModel` (never a private
  offset), updates it immediately, and emits `ScrollAction::ScrollTo`;
  `on_pointer` captures a preserved drag anchor (re-clamped each move so a
  mid-drag range change stays valid), presses end buttons for line steps and
  the track for page steps, `repeat()` is the one-shot-timer auto-repeat seam
  the owner drives (no polling loop), `on_key` steps the orientation's arrows
  plus Page/Home/End on a focused bar, and `wheel` steps one line per tick
  along the bar's axis; `part_at`/`geometry` expose the shared layout. Content
  changes are never animated (reduced-motion correct). 21 host tests.
- **Drawn controls: window furniture — DONE.** `lib/controls::window` draws the
  WM-owned frame family over the shared paint core: `WindowControl` (the compact
  Close/Minimize/PutToBack/SizeToggle command buttons — command glyphs drawn
  here like `paint_chevron`, reading by shape without colour; the size toggle
  shows/names its *next* action), `TitleBar` (four controls on either edge with
  close outermost, untrusted-title sanitiser + ellipsis truncation, drag region
  that activates on press and begins a cooperative move past the threshold, a
  press over a control routing to it and never dragging, Left/Right focus nav +
  Space/Enter), `WindowFrame` (active/inactive/attention Frame Rim with a doubled
  inner line and a bounded static attention dot — activation never changes the
  client origin/outer size — plus the furniture hit map classifying every point
  as `Client`/`TitleBar`/`WindowControl`/`ResizeEdge`/`Frame`/`Outside` so the
  client never receives furniture input), `ResizeGrabber` (grip teeth, drag
  capture with Escape-cancel, disabled when non-resizable/maximized, hit region
  kept clear of scrollbar thumbs), and `ScrollCorner` (the inert neutral
  junction plate). Typed `WindowControlAction`/`TitleBarEvent`/`ResizeEvent`; the
  WM performs the cooperative operation and enforces authority (§13 denied keeps
  the value + lock bead). 37 host tests (glyphs, activation, disabled/denied,
  inactive muting, high contrast, scale, both themes, title layout/sanitise/drag/
  routing/focus, frame hit-map isolation + resize edges + activation-invariant
  client + rim/attention, grabber drag/escape/non-overlap, scroll corner).
- **Drawn controls: shell surfaces — DONE.** `lib/controls::shell` draws
  `Notification`, `TaskbarItem`, and `TraySignal` over the shared paint core.
  `TaskbarItem` and `TraySignal` live only on the taskbar and are **bar-seated**
  (`PlateSeating::Bar`, the one shared seating rule in `paint::FrameColors`):
  they wear no rim in any state and no plate at all while resting, so the icon
  strip reads as one bar; `IconButton` carries the choice (`seated`) because it
  is the only family that appears on both a panel and the bar.
  `Notification` composes a `Card` plus a source attribution: informational
  (quiet rim), background job (Heat Seam), warning (warning rail via the shared
  `dominant_color`), recovery (bead), and denied (Authority Mark beside the
  source) all read from its composed state; its actions are footer `Button`s
  (typed `NotificationAction`). `TaskbarItem` combines an icon+label identity
  with a `TaskVisibility` (Closed/Running/Active/Minimized) window state, stated
  by the lower **presence mark** — full-width accent seam for the active window,
  a short muted mark for one merely running, nothing for a closed pin; minimized
  adds a recessed plate + a non-colour tick — plus the activity Heat Seam and an
  attention/recovery/denied Signal Bead, failing closed on a denied click (typed
  `TaskbarItemAction`). `TraySignal` is a calm glyph capsule that stacks
  severity-ordered mini beads (denied > recovery > warning > complete), shows a
  leading pressure rail and lower Heat Seam, and
  expands on hover/focus to an instrument readout (state name, value, one safe
  action) the owner positions (typed `TraySignalAction`). 26 host tests.
- **Drawn controls: decision surfaces — DONE.** `lib/controls::decision` draws
  `Dialog`, `Tooltip`, and `HelpTip` over the shared paint core. `Dialog` is a
  modal choice surface with a title, message, optional inline reason, and a
  right-aligned action-`Button` row: Action Warmth is honest (an action is warm
  only when its role is Recommended/Primary; a destructive action carries the
  danger/confirmation posture), and a blocked action shows the §13 Authority
  Mark rather than a plain disabled look (typed `DialogAction`, fail-closed on a
  denied click). `Tooltip` is a short anchored affordance hint. `HelpTip`
  explains why an action is unavailable or recommended (reason tone by role)
  with one optional safe next-step `Button` (typed `HelpTipAction`). 17 host
  tests.
- **Switchboard screen composition — DONE.** The Switchboard *screen* is
  application-specific composition, so it lives in the application
  (`userland/gui/switchboard::view`, `plans/NEW-SWITCHBOARD.md` S1);
  `lib/controls` holds only controls any surface may reuse. The screen
  assembles Switchboard purely from the shared controls — the
  `WindowFrame`/`TitleBar`/`ResizeGrabber`/`ScrollCorner` furniture, a `Tabs`
  strip, `ListRow`/`Card`/`Panel`/`Button` content, an `ActionRail` beside the
  list, and one vertical `ScrollBar` over the Stage A/B scroll engine —
  proving no surface needs custom chrome. It
  turns a typed `SwitchboardModel` (Task/Job/Recovery/Resource/Service/System
  view models) into controls and emits a typed `SwitchboardAction`; the client
  can never receive furniture input (`furniture_at` over the frame hit map), a
  denied action fails closed and renders `DeniedByAuthority`, a force action
  carries the destructive-confirmation posture, and the mouse wheel, thumb,
  end buttons, track paging, and keyboard all scroll the active section (offsets
  are per-section, re-clamped on section switch/resize). A host opens the panel
  on a chosen section (`select_section`) and
  refreshes live data in place (`set_model`, preserving section, per-section
  scroll, focus and any in-flight drag). Its own host tests take the controls'
  shared heavy-contrast fixture through the `tairix-controls` `test-support`
  feature, so an application's render tests exercise the same two contrast
  axes as the controls with no second copy of the fixture. The Reactive Alloy
  control set is now complete (Stages A–F).
- **Instruments: the metric track and `Chart` — DONE.** There is exactly one
  reading-with-a-track: `lib/controls::metric`'s `MetricTile` under
  `MetricInstrument::Track`, which draws the rounded track tinted by the
  resource's semantic rail over the shared paint core and the shared
  rail-colour lookup `Card` uses. `state::MeterValue::Unmeasured` makes an
  unmeasurable resource unrepresentable as a real zero, so a denied or absent
  query renders a quiet groove rather than a fabricated `0%`.
  `lib/controls::chart` is its history counterpart (spec §11.35): a bounded
  oldest-to-newest permille series plotted as a line with a quiet filled body,
  mapping its readings across the *whole* box it is given rather than an
  instrument groove, and drawn through the one shared stroke path in
  `lib/raster`. Both read-only: no input, no action.

### Stage 7 follow-up — the desktop pinboard (`plans/PINBOARD.md`)

**Status: in progress.** The desktop backdrop becomes a real pinboard: a
wallpaper drawn behind everything, the user's `Desktop` folder over it, a
backdrop context menu, and a per-user settings document the chooser app
edits. `plans/PINBOARD.md` is the binding design and carries the
deliverable list (P1–P10) and its current state; it is not repeated here.

Load-bearing decisions a future contributor needs:

- **`lib/wallpaper`** (new `lib/*` crate, registered in `AGENTS.md` §3) owns
  the settings document (`<home>/Settings/Pinboard/pinboard.conf`), the
  wallpaper catalog, the fit geometry, and the five shipped masters in its
  `assets/`, which `tools/syshelp` plants at `/System/Graphics/Wallpapers`.
  The default is `tairix-dark.jpg`.
- **`lib/image` decodes JPEG** (baseline and progressive) as well as PNG,
  with a reduced-scale decode so an 8.3-megapixel master is never materialised
  whole; the wallpapers are JPEG and a 1 GiB machine must still draw them.
- **`lib/raster::resample`** is the one image resampler, shared by the icon
  and wallpaper paths.
- **`lib/sandbox`'s `imagerender`** (renamed from `iconraster`) gained the
  wallpaper prepare/band/release ops, so untrusted wallpaper bytes are
  decoded only in the capability-empty worker and a screenful of pixels
  crosses the fixed 8 MiB frame bound in bands rather than raising it.
- **The desktop session is the settings document's only writer.** The
  chooser (`wallpaper.app`) and the backdrop menu both *ask*, over the
  reserved seat-scoped `PINBOARD_ENDPOINT`, whose request carries the
  rendered document itself rather than a second encoding of the model.
- **The icon arrangement is a setting**, which is why
  `tairix_browse::GridFlow` gained `ColumnsFromLeading` beside its existing
  mirror image.

---

## Stage 8 — Installer and Image Builders

**Dependencies:** Stages 5, 6 (and 7 for the graphical installer path).

**Deliverables**
- `userland/system/installer` with text and graphical front-ends sharing one core
  library. Functions per `AGENTS.md` §11 and lays out the filesystem per
  `AGENTS.md` §16: exactly `/System`, `/Users`, `/Apps`, `/Storage`; no
  legacy POSIX top-level directories; mount flags as specified in §11.3
  and §16.3; expert mode refuses any reserved name. The secure default
  lays out encrypted root **and** encrypted swap (`AGENTS.md` §4, §11);
  plaintext swap is never offered, including in expert mode.
- Kernel swap subsystem: when the process/VM model gains a pager, swap is
  brought up only through the encrypted-swap layer keyed by an ephemeral
  per-boot key from the platform RNG (`AGENTS.md` §4, §19.2). The kernel
  refuses to activate an unencrypted swap device and fails closed; the key
  is discarded on shutdown and never persisted.
  - **Encrypted-swap layer — DONE (landed ahead of Stage 8).**
    `kernel/mem::swap` is the cryptographic envelope the pager must route
    through: `EncryptedSwap` is the *sole* way to use a `SwapBackend`
    (plaintext swap is unrepresentable, `AGENTS.md` §2.11 — fail closed by
    construction), sealing each page with `lib/crypto`'s new
    ChaCha20-Poly1305 AEAD wrapper (`aead::seal`/`open`). The `SwapKey` is
    ephemeral, drawn from an injected `EntropySource` (the §19.2 RNG seam),
    zeroed on drop, and never persisted. Record layout
    `nonce(12) ‖ tag(16) ‖ ciphertext(4096)`; per-write `salt ‖ counter`
    nonce (exhaustion fails closed); slot index bound as AAD; `load` zeroes
    the caller's buffer on every failure. 16 unit tests + a §19.6 fuzz
    harness (`tests/fuzz_swap.rs`); `lib/crypto` gains 7 AEAD tests incl.
    the RFC 8439 vector. **Still pending:** the pager that calls
    `store`/`load`, wiring the now-landed platform-RNG `EntropySource`
    (`.junie/PREREQUISITES.md` P-0) to the ephemeral swap key, the swap-device
    backend driver, and the `CAP`-gated activation syscall — all Stage 8.
- `tools/mkimage` producing:
  - `images/tairix-x86_64.iso` (hybrid BIOS/UEFI). Booting this on real
    firmware needs the first-party, Rust-only boot chain (no GRUB, no C):
    the pure loader core `lib/bootload` (ELF→`LoadPlan`, landed) plus the
    per-firmware `boot/*` shells, handing off through the kernel's existing
    multiboot2 entry. Staged in `plans/BOOTLOADER.md`; the GPT/ESP whole-disk
    builder is B4 there, and `plans/ARCHSUPPORT.md` A1 depends on it.
  - `images/tairix-aarch64-rpi.img` — **DONE (landed ahead of Stage 8 as
    `plans/PI.md` P9).** `tairix-mkimage` (lib + bin) authors the image
    in pure Rust via the one-step `cargo xtask image --target aarch64-rpi`
    (or `build --target aarch64-rpi`): MBR, FAT32 boot partition (pinned,
    checksummed Pi firmware inputs per `tools/mkimage/firmware.lock` —
    fetched automatically from the manifest's pinned source when not
    operator-staged, every download checksum-gated —
    generated `config.txt`, flattened `kernel8.img`), and an encrypted
    ARXFS root with the §16 skeleton, both laid down by the real
    in-tree drivers. Docs: `docs/src/install/raspberry_pi.md`. The emitted
    image boots a real Pi 4 into user mode (operator metal acceptance,
    `plans/PI.md` P9). The store also ships the signed virtio-input
    keyboard/pointer bundle (`drivers/input/virtio_kbd`, unbound on the
    Pi tree, §18.4), so the same image is interactively testable on QEMU
    `virt`: **`cargo xtask run --target aarch64-rpi
    [--profile debug|installer] [--cpus N]`** builds the image and boots
    it windowed (`-device ramfb` + virtio keyboard/mouse + the image as
    virtio-blk root; `Runner::run_interactive` in `tools/qemu`). The
    aarch64 framebuffer boot console renders on `virt` through its
    fw_cfg/ramfb fallback (`video::configure_ramfb` over `lib/fwcfg`);
    the invoking terminal is the guest serial console for the
    encrypted-root unlock (`docs/src/platform/aarch64.md`).
  - `images/tairix-riscv64.img`.
  - `images/tairix-web/` static tree.

**Tests**
- End-to-end QEMU install: build image → boot → run installer → reboot →
  log in as the created user → verify permissions and partition layout.
- Browser headless test for the `wasm32` image.

**Docs**
- `docs/src/install/{x86_64,raspberry_pi,riscv64,web}.md`.

---

## Stage 9 — Security Hardening and Audit

**Dependencies:** all earlier stages feature-complete.

**Deliverables**
- Threat model document (`docs/src/security/threat_model.md`).
- Fuzz harnesses for every parser (filesystem, ABI, manifest, IPC).
- Sandboxing review of every driver currently running in-kernel; move to
  user space if at all possible.
- `cargo audit` + `cargo deny` clean across the workspace.
- Reproducible builds (`tools/xtask repro`).

**Tests**
- Fuzz campaigns run in CI for a bounded time on every PR.
- Penetration test scripts under `tests/security/`.

**Docs**
- `docs/src/security/{threat_model,hardening,audit_log,reporting}.md`.

---

## Stage 10 — Release Engineering

**Dependencies:** Stage 9.

**Deliverables**
- Versioning policy (semver applied to ABI, distinct from product version).
- Release checklist in `docs/src/release.md`.
- Signed releases of all four images.
- Upgrade path documentation (`abi-vN` → `abi-vN+1`).

---

## Open defect — intermittent root-unlock QEMU vertical failure

**Status: open (escalated under §2.18/§15.7; observed once, not yet reproduced
in isolation).**

Under parallel `cargo xtask ci` load, an aarch64 root-unlock QEMU vertical can
fail: after the scripted wrong-prefix attempt is refused (expected), the
*correct* fixture passphrase line is also refused, the prompt re-appears, and
the run times out. Suspected mechanism: the interactive unlock's timed
anti-brute-force window is a per-attempt deadline, so a passphrase line whose
serial bytes are delivered slowly under host load can straddle the window and
be split into two partial (refused) attempts. Work remaining:

- Reproduce under controlled load; confirm whether the attempt window (or a
  console-read deadline) can split a mid-flight line.
- If confirmed, restructure the window so it never truncates an in-progress
  line (e.g. arm the penalty window only between attempts, or bound the
  *inter-byte* gap rather than the whole line) — without weakening the
  anti-brute-force defence (§2.17) — and land the fix with a regression test
  that types a slowly-delivered line (§7).

## Defect — spawn-session x86_64 failure (user OOM + wrong write count)

**Status: root cause found and fixed** (the x86_64 syscall return path and
the zero-page frame wedge below); the remaining piece is the ring-3
fault-handler decision at the end of this section.

**Fixed (was CI-host-only): login spawn refused `NoSpace` (err 15) — the
zero-page frame wedge.** The producers' formerly silent pre-build refusal
sites now audit a stable `cause` plus the allocator's free-frame margin
(shared `kernel_core::{refuse_spawn, refuse_admit}` helpers, all three
ports), and the instrumented CI run named the site:
`cause=page_table_frames_exhausted free_frames=47232` — a page-table frame
refusal with ~185 MiB free. Root cause: the x86_64 firmware map reports the
low BIOS region (including physical page 0) usable, and the spawn producer's
page-table frame source translates each drawn frame through the low
**identity** direct map — for frame 0 that translation is the null pointer,
which `NonNull` cannot represent, so the source hands the frame back and
fails closed. The buddy allocator hands out the **lowest** free index first,
so once frame 0 enters the free lists (timing-dependent — which is why only
the CI host's interleaving hit it) every subsequent page-table allocation
re-draws frame 0 and fails forever while `free_frames` stays huge. Fix:
`FrameAllocator::new` (`kernel/mem/src/frame.rs`) never enrolls the zero
page, even when firmware reports it usable — it stays reserved like
firmware-reserved RAM on every port (it is also the PC real-mode IVT/BDA
page), excluded from `usable_frames`. Regression tests:
`zero_page_is_reserved_even_when_firmware_reports_it_usable` and
`map_of_only_the_zero_page_has_no_usable_frame` (frame.rs), and
`direct_identity_rejects_the_null_translation` pins the hazard the
reservation defends against (phys.rs).

**Root cause.** The x86_64 `IA32_LSTAR` entry stub
(`kernel/arch/x86_64/src/syscall_entry.rs`) tore its on-stack argument array
down with a bare stack drop (`addq $48, %rsp`) instead of popping the values
back into `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`, so after `sysretq` those six
registers held kernel dispatch residue while the user-side trap stub
(`lib/abi-trap`) declares only `rax`/`rcx`/`r11` clobbered. Consequences:
a call-site-dependent miscompilation of every syscall wrapper (the compiler
legitimately re-uses "unchanged" registers — e.g. `hw_tree_read`'s
`min(ret, buf.len())` computed against residue, observed locally as a
16-byte snapshot "returning" 0), and a kernel-register information leak into
ring 3. The residue *values* are environment-dependent kernel state, which
is why the symptoms differed per machine: on the CI runner they surfaced as
the "memory allocation of 8 bytes failed" user OOM and `stream_write` counts
equal to the preceding panic report's length; locally as devmgr's tree read
appearing to return zero bytes, ending its loop fail-closed after the first
evaluation (the "devmgr exits instead of parking in `hw_tree_wait`"
observation — the seam never refused; the *return* was corrupted).

**Fixed.** The stub now restores all six argument registers before
`sysretq` (`docs/src/platform/x86_64.md`). Regression vertical:
`tairix-test-syscall-regs-qemu-x86_64` enters ring 3, loads sentinels into
the six argument registers and the six callee-saved registers, round-trips a
real `syscall`, and verifies every register plus the returned `rax`
(fails against the pre-fix stub, passes with it). devmgr additionally now
states its reason on an abnormal exit — `TREE_SEAM_FAILED` (13_009) with the
errno through the kernel log — instead of a silent status-1 exit (fail
loud), so a future tree-seam failure is attributable from the transcript.
`tairix-rt`'s `stream_write` retains the earlier defence: a negative
(`-errno`) return folds to a zero-length write and the count is clamped to
the buffer length, with regression tests.

Work remaining:

- Decide whether the production x86_64 boot path must install a ring-3 fault
  handler that kills the faulting task (fail closed, log, reap) instead of
  taking the whole machine down through the arch default (the status-35 exit
  the CI panic storm ended in); land it with a QEMU regression test if so.
  The same decision applies to the aarch64/riscv64 ports (their production
  boots install no fault handler either); the kill path should be one shared
  kernel/core definition (current-task lookup → recorded exit status →
  `reschedule_current(cpu, Exit)`), with only the per-port trap plumbing —
  and the x86_64 swapgs-parity fixup the `#PF` entry needs before it may
  reschedule — arch-specific.

## Cross-cutting Tasks (run continuously alongside the stages)

These never "finish"; they are part of every PR.

- **Tests:** new code ships with tests; failing tests block merge.
- **Docs:** every change updates rustdoc and the relevant `docs/src/` page.
- **Lints:** `clippy -D warnings`, `fmt --check`, `cargo deny` always pass.
- **Coverage:** thresholds from `AGENTS.md` §7 are enforced.
- **ABI checks:** `cargo xtask abi-check` runs on every PR; ABI changes
  require a version bump and a migration note in `docs/src/abi/`.
- **Modularity checks:** `cargo xtask deps-check` and `cargo xtask
  cfg-check` run on every PR and enforce `AGENTS.md` §17 (layering,
  concrete-scheduler naming, optional-desktop boundary, and
  target-conditional-`cfg` confinement). See the §17 burn-down below.
- **No duplication:** code reviewers reject duplication; refactor into
  `lib/` instead.

---

## §17 Modularity Enforcement and Burn-down

**Status: complete.** Enforcement is delivered and the burn-down is finished.
`cargo xtask deps-check` and `cargo xtask cfg-check`
(`tools/xtask/src/commands/{deps_check,cfg_check}.rs`) implement the §17.5
checks, run in `cargo xtask ci`, and `cargo xtask build --headless` exercises
the §17.3 headless image (`docs/src/architecture/modularity.md`).

Every grandfathered violation the pre-§17 tree carried has been removed: both
the `deps-check` (§17.4/§17.1 layering, no concrete-scheduler naming outside
`kernel/sched/*`) and `cfg-check` (§17.2 target-conditional confinement)
grandfather lists are now **empty**. Notably resolved: the pluggable scheduler
(`SchedulerPolicy` in `kernel/sched/api` + sibling impls, single selection
point in `kernel/core`), the Arch HAL migration, the `kernel/tairix-kernel`
binary no longer naming a concrete target, virtio protocol/host relocation off
the bus driver, and the heterogeneous-CPU `core_class` (Intel + AMD CPUID
paths). No new violation may be introduced.

**Arch HAL surface record — page-table teardown (the `plans/APPS.md` I2
reclamation).** Two in-place extensions of existing §17.2 slices (no new
slice; the closed enumeration in `AGENTS.md` §17.2 is unchanged):
`tairix_arch_api::frames::PageTableFrames::free_table` is the teardown half
of the frame-source seam (the allocator-backed `FrameTableSource` recycles;
the per-port boot pools retire without reuse), with the one shared post-order
`frames::reclaim_hierarchy` walk every port's teardown drives; and
`tairix_arch_api::mmu::AddressSpace::reclaim_table_frames` (defaulted no-op
for backends with no allocator-drawn tables) returns a dead space's root +
intermediate table frames, overridden by all three paging ports. Each port
also publishes a set-once **park root** (the permanent boot translation) that
`park_kernel_root` re-installs; the dispatcher parks a CPU off a user root at
every task suspend (`KernelArch::park_translation` →
`kernel/core::install_park_translation`), the invariant that makes teardown
SMP-safe. Behaviour and tests are recorded in
`docs/src/architecture/memory.md` and `plans/APPS.md` I2.

---

## §19 Threat Model and Hardening Burn-down

**Status:** the implementable portion is complete; the remainder is
stage-blocked, not deferred by choice. §19 supersedes the loose Stage 9
deliverables (where they conflict, §19 wins) and follows the same shrink-only,
fail-closed discipline as §17; each item lands with its own tests + docs.

**Standing directive (owner):** every *independent* burn-down item
(1, 3, 4, 5, 6, 7, 8, 9, 11, 13) is **landed and verified green**. Items
(2, 10) are **stage-blocked** and carry a binding **[DO IMMEDIATELY ON
UNBLOCK]** order — the session that lands the prerequisite stage must
complete the matching §19 item before other Stage work proceeds; item 12
stays aspirational per charter §19.7/§19.8.

Landed (done):
- §19.1 side channels — per-port `SideChannelMitigation` honest profiles
  (x86_64 `lfence`+`verw`, aarch64 `csdb`, riscv64 `fence`, wasm32 host-owned);
  `lib/crypto` `ct_eq` constant-time-under-`-O3` test in `ci`.
- §19.2 W^X/ASLR/CFI — `rxe` loader (`lib/abi/src/rxe.rs`: R/RX/RW only, PIE
  required, CFI tag vs syscall-interface hash) + `kernel/mem` `map_image`/
  `build_process_image` (segment fill, user stack, startup vector) + the
  `EnterUser` HAL primitive (riscv64 `sret`, aarch64 EL0 `eret`, x86_64 `iretq`).
- §19.3 supply chain — `cargo xtask sbom` (deterministic CycloneDX, unsigned)
  + `cargo xtask supply-chain` (source-hash allow-list + advisory SLA), in `ci`.
- §19.4 audit log — SHA-256 hash-chain core (`lib/log` `chain.rs`, per-stream)
  + the on-disk segment container (`lib/log` `segment.rs`: self-checksummed
  header, chained records, optionally-sealed footer, forward-scan recovery) +
  the closed `Stream` set + the logical record model (`lib/log` `record.rs`:
  the SYSLOG §5 record body — effective level, per-CPU seq, per-record
  `WallClockReading`, attested `Origin`, source name, caller content, and
  `data.*` fields — over the shared named-field codec) + the segment-local
  string dictionary (`lib/log` `dict.rs`: a back-reference codec compressing
  the record's low-cardinality provenance/message strings, bounded
  promote-on-repeat, fail-closed, no separate on-disk block) + the authority
  model (`lib/log` `authority.rs`: system-derived `SourceName` from the
  attested `Origin`, reserved-source-prefix spoof screening, and
  `resolve_stream` effective-stream assignment) + the early-boot ring buffers
  (`lib/log` `bootring.rs`: `BootRing`, a bounded per-CPU allocation-free FIFO
  holding the same record body plus the `cpu_seq`/monotonic import must
  preserve, evict-oldest with a contiguous `LossRange` for a trusted loss
  record) + the record-ingress admission core (`lib/log` `ingress.rs`:
  `Ingress` owns the per-stream append `seq` and is its single writer; `admit`
  combines an attested `Origin` with the caller's stream/source/level requests
  via `resolve_stream` + `derive_source` + reserved-source spoof screening,
  assigns the effective level and one append seq, and returns an `Admission`
  that builds the record body with the caller requests preserved as claims) +
  the architecture-neutral persistence **engine** (`lib/log` `journal.rs`:
  `Journal<S: SegmentStore>` owns the `Ingress` + per-stream segment state and
  drives the segment lifecycle — `commit` encodes an admitted record and
  appends it to its stream's open `SegmentWriter`, rotating
  close→seal→`store_segment`→reopen a segment chained on the closed one's
  `segment_hash` when a buffer fills, seeding each segment's `first_seq` with
  the record's reserved seq so the segment chain and `Ingress` stay in
  lockstep; `import_boot` drains a `BootRing` into `boot` and authors one
  trusted `journal`-stream loss record for an evicted `LossRange`; `flush`
  closes all open segments; fail-closed throughout — an over-cap/invalid
  record is rejected whole and an audit/security segment cannot close without
  the seal key; `SegmentWriter::finish` now returns a `FinishedSegment`
  reclaiming its buffer for reuse). The journal-ingress wire ABI
  (`lib/abi/src/log_ingress.rs`: `LOG_INGRESS_ENDPOINT` + `LogIngressRequest` +
  status-word reply — a service IPC protocol, no C header), the trusted spoof
  security record (`Journal::note_spoof`), and the architecture-neutral service
  dispatch core (`userland/system/journald`: `serve` admits an attested
  request and commits it, `store` derives the `/System/Logs/<stream>/` path)
  have landed. The freestanding **service binary** (`journald` `Run`) + FS-backed
  `SegmentStore` have also landed: it reads its identity, binds
  `LOG_INGRESS_ENDPOINT`, and writes each closed segment as its own immutable
  `/System/Logs/<stream>/<id>.seg` file (placement read from the segment's own
  header, `fs_sync`'d, fail-closed). Two prerequisites landed with it: the
  unprivileged **`self_origin`** syscall (no. 68 — a task reads its own attested
  `Origin`, for the trusted records the journal authors itself) and the
  non-secret per-installation **machine-id** as the single on-disk source of
  truth (`/System/Security/MachineId`, `AGENTS.md` §16.2; mkimage bakes a random
  one, journald reads it for the stream genesis). Each on-disk record block now
  also carries its own monotonic ordering time (§5.1) — previously
  `append_record` dropped it, keeping only the footer's first/last bounds — read
  and validated fail-closed by the segment reader and exposed on
  `RecordBlockRef`. The **boot console renderer** (`lib/log` `render.rs`,
  §8.2) has landed on top of it: `render_line` formats a decoded record plus its
  monotonic time into the canonical `[monotonic] level source[component]:
  message key=value` line, escaping every control character in caller-controlled
  text so it cannot inject terminal escapes or forge lines (control-byte-free
  output proven by the `fuzz_render` harness). The **rich renderers**
  (`lib/log` `report.rs`, §8.3) have landed on top of it: `render_json` /
  `render_markdown` / `render_table_row` (over a `RecordFrame` a reader fills
  from a segment header + record block) are the structured views the `log`
  tools render, each separating system-attested metadata from caller content
  and showing a caller's *requested* privileged source/stream inertly as a
  claim; caller text is escaped so the JSON is valid and the JSON/table output
  is control-byte-free (proven by the `fuzz_report` harness). The one stream
  string spelling is now `Stream::name()`. The **`log` CLI read/render/verify
  library** (`userland/shell/log`, crate `tairix-logtool`, §14) has landed on
  top: a host-tested seam-library (`SegmentSource`/`Output`) with
  `show`/`report`/`export` (over `render_line`/`render_json`/`render_markdown`/
  `render_table_*`) and `verify` (over `verify_segment`), reading a stream's
  segments one image at a time (oldest first, bounded memory), decoding with a
  per-segment `DictionaryView`, and failing closed on corrupt/tampered
  segments or a missing seal key for `audit`/`security`; it added
  `Stream::from_name` in `lib/log` for the stream operand, mirroring the
  `cat`/`ls` tool-library precedent. Remaining SYSLOG work: the `log`
  freestanding `Run` binary + QEMU vertical (and `tail`/`find`/`boot`/`expire`),
  boot-ring import (needs a kernel-side boot ring + drain syscall that do not
  exist yet; per-CPU gap detection lands with that producer, since `journald`
  assigns `cpu_seq` contiguously and cannot gap in steady state), retention,
  the QEMU vertical (launch journald under `init`), the kernel
  `SystemIdentity`↔machine-id unification, and anchors (see `.junie/SYSLOG.md`).
- §19.6 fuzzing — `cargo xtask fuzz` over all in-tree harnesses (`--quick`/
  `--soak`), fail-closed.
- §19.7 verified core — Bronze proptest models for `lib/caps`/`kernel/sec`/
  `kernel/ipc`/`kernel/syscall` via `cargo xtask proptest` + `spec-review`.
- §19.10 memory tagging — `MemoryTagging` HAL + the `kernel/mem` slab software
  UAF tag-check (on-by-default floor everywhere).

Unblocked — in progress (`.junie/fstree-next-plan.md` S8):
- Item 9 — §19.5 parser sandboxing (minimum-capability sandbox process
  model). Its prerequisite (Stage 6) is complete. The **kernel sandbox-spawn
  primitive is landed** (S8a): `SpawnAttach` carries a `flags` word whose
  `SPAWN_FLAG_SANDBOX` bit admits the child as a parser sandbox — the block
  is canonical only with fully explicit `Closed`/`Handle` wires, an
  inherited credential, and no console index (one fail-closed definition in
  `SpawnAttach::parse`); the child's capability record is branded
  `as_sandboxed()` (all three sets forced empty regardless of manifest,
  `delegate`/`apply_token` refuse it outright), and the syscall dispatcher
  confines a sandboxed task to the closed `sandbox_allows` list (yield,
  exit, stream_read/write, fs_read/write/close, mem_map/unmap), audited on
  denial. The **user-space seam is landed** (S8b): `lib/sandbox`
  (`tairix-sandbox`) is the one typed request/reply path a program runs a
  parser through — length-framed protocol over a `Channel`, worker `serve`
  loop over a total `Service`, and parent-side `ParserSandbox` crash
  containment (typed error, dead worker reaped and replaced, stable
  `EventId(6000)`/`EventId(6001)` events, §19.4) — with the production
  transport (`RtLauncher` spawns the program's own binary in a worker role
  via `SpawnAttach::sandbox` over pipes), a public in-process loopback
  fake for host tests, and the `lib/binfmt`/`lib/disasm` decode service
  behind it (fail-closed client-side reply validation; `fuzz_sandbox` in
  `cargo xtask fuzz`). Proven end to end by the aarch64 QEMU vertical
  (`tests/integration/sandbox_program` + `sandbox_qemu_aarch64`): decode
  of valid/malformed inputs through a real sandboxed worker, real-process
  crash containment, and the syscall wall probed from inside. Docs:
  `docs/src/security/sandbox.md`. The **parser sweep is complete** (S8c):
  every in-tree parser of untrusted input is enumerated below with its
  §19.5 posture, and every live userland parse of foreign data runs
  behind the facility.
  - **Behind the facility:** `lib/binfmt` + `lib/disasm` (the S8b decode
    service, above), and the `lib/help` document parse+render
    (`tairix_sandbox::helpdoc`): `man` locates a *foreign* bundle's
    document with its own file authority (`tairix_help::load_raw`, the
    same one locale walk `load` uses), hands the raw bytes to a sandboxed
    worker (its own binary re-spawned, `CAP_PROC_SPAWN` in its manifest),
    and re-validates the reply against the closed render-op whitelist
    (printable text, line feeds, bold/underline SGR) — a hostile reply is
    refused whole, a document-parse error round-trips typed
    (`fuzz_sandbox` covers both directions). A command's `-h` render of
    its **own** bundle's document (`lib/help` `own_short_help`) parses
    content from its own signed bundle — the same trust as its own code —
    under the engine's fail-closed bounds.
  - **The process is the sandbox:** the `userland/net/icmp` decode engine
    (ARP/IPv4/ICMP) is bounded, total, and fuzzed (`fuzz_parse`); its
    service process, when spawned, holds only its NIC-queue authority — a
    dedicated minimum-capability address space per §19.5.
  - **No runtime untrusted consumer yet (sandboxed with their consuming
    stage):** `lib/svg` (with `lib/icon`/`lib/cursor`) decodes only
    OS-authored theme assets today and has no runtime file-reading
    consumer; the §10 sandboxed asset decode lands with the desktop asset
    pipeline (`plans/DISPLAY.md`). The fstree disassembly viewer (S9)
    runs only over the S8b decode service.
  - **Outside §19.5's userland-parser scope, hardened in place:**
    kernel/boot-side parsers of platform data (`lib/fdt`, ACPI,
    `lib/partition`, `lib/fsprobe`, the filesystem drivers' on-disk
    formats) are fail-closed, §24.4-bounded, and fuzzed where reachable;
    their process isolation is the user-space driver model
    (`plans/fixdrivers.md`, `plans/DRIVES.md`). Parsers of the user's own
    typed input (`lib/vt`, `lib/glob`, `lib/path`, `lib/resref`,
    `lib/cmdres`) parse the caller's own keystrokes, not foreign data,
    and stay fail-closed and bounded.
  Its first consumers exist: `lib/binfmt` (`tairix-binfmt`, done —
  `.junie/fstree-next-plan.md` S6) is the read-only executable-container
  decoder: typed, borrowed, fail-closed views of the `rxe` load image +
  manifest summary (decoded through the `lib/abi` types —
  `LoadImage::parse_for_inspection`, `decode_capability_ids` — so
  inspection and the load path never diverge, and the CFI tag is reported,
  never compared), ELF64 (header/phdrs/sections/names/symbols, lazy and
  bounds-checked, extended numbering refused), and wasm module structure
  (section directory + code-body framing over strict LEB128). It is
  `no_std`+`alloc`, `#![forbid(unsafe_code)]`, capped per §24.4, unit-
  tested with truncation/mutation matrices, and fuzzed (`fuzz_rxe`/
  `fuzz_elf`/`fuzz_wasm` in `cargo xtask fuzz`). `lib/disasm`
  (`tairix-disasm`, done — `.junie/fstree-next-plan.md` S7) is the sibling
  instruction-decoder crate: pure slice+address decoders for the four
  Tier-1 ISAs (riscv64 RV64GC incl. C, aarch64 A64, wasm code bodies,
  x86_64 one-/two-byte maps with the 15-byte cap) over one shared `Insn`
  vocabulary — forward progress on any input, undecodable bytes rendered
  honestly (`(bad)`/`.inst`), validation-bounded per §24.4, per-ISA
  conformance tables, and fuzzed (`fuzz_riscv64`/`fuzz_aarch64`/
  `fuzz_wasm_isa`/`fuzz_x86_64` in `cargo xtask fuzz`). The fstree
  disassembly viewer (S9) runs both only inside the S8 sandbox.

Stage-blocked **[DO IMMEDIATELY ON UNBLOCK]**:
- Item 2 — §19.4 signed log anchors + per-service `CAP_LOG_WRITE` partitioning
  (needs a private-key signing API, Stage 2; + persisted log store, Stage 5).
- Item 10 — §19.2 stack-canary/shadow-stack + per-arch live fault fix-up and
  remaining §19.3 `build --reproducible` / no-post-install-fetch (Stage 6/8).
- §19.1 KPTI/IBPB and §19.10 auto-enable Arm MTE on `FEAT_MTE` close with the
  Stage 6 user/kernel boundary + page-table work.
- Item 12 — §19.7 Silver (TLA+) / Gold (Verus) — aspirational per charter.

---

## §20 / §21 ABI Compliance (`stdinfo` + 64-bit-native time)

**Status: complete.** §20 (`stdinfo`) and §21 (64-bit time) compliance passed
before continuing Stage 6:
- §21 canonical types `Time64`/`Duration64` (`lib/abi/src/time.rs`, 12-byte LE,
  `timespec64` analogue); narrowing to legacy fields is checked and fails with
  `Errno::TimestampOutOfRange` (no silent truncation/wrap).
- §21 ABI migration: `sysinfo::Uptime` moved to `{ since_boot: Duration64,
  boot_time: Time64 }`; all call sites + fuzz harness updated.
- §20 `stdinfo`: `STDINFO_FD = 3`, closed `StdInfoRecord` (closed `StdInfoKind`,
  no synonyms), `no_std`/alloc-free `write_jsonl` (fail-closed on small buffer).
- ARXFS stores the four §21 timestamps as true `Time64` via a separate
  versioned `FilesystemTimestamps` trait (not a widening of read/write, §2.4);
  inode record reshaped, `FORMAT_VERSION` bumped, clock seam defaults to epoch
  (never panics).
---

## TSC hardening & untrusted-timer resolution (§19.1)

**Status: complete.** Two TSC/timer side-channel risks closed (no `clock_get`
signature change, so the syscall hash is untouched):
- Validate the TSC (x86_64): `kernel/arch/x86_64/src/tsc.rs` decodes
  Invariant-TSC support (CPUID `0x8000_0007` EDX bit 8); the boot pipeline logs
  it and fails closed (`TscNotInvariant`) before bringing a second CPU online on
  a part lacking it. Frequency is still measured against the PIT, never trusted
  from CPUID.
- Coarsen time for untrusted code: new `CAP_TIME_HIRES` (`CapabilityId 16`);
  `clock_get` returns raw ns only to holders, else floored to
  `COARSE_CLOCK_GRANULARITY_NS` (1 µs) via the single `coarsen_clock_ns` helper,
  preserving the per-CPU monotonic contract `irq_wait` needs.

---

## §24 Resource Limits and Scalability  **[IN PROGRESS — L1+L2+L3a+L3b+L4a+L4b landed; the §24.1 sweep (heap span table, supplementary-group ceiling, spawn fan-out, all-arch per-CPU handle bookkeeping, growable AND shrinkable kernel-stack arena, and the per-arch secondary-bring-up bound on all three bare-metal ports) is complete]**

**Status: L1 (ABI) + L2 (kernel enforcement) + L3a (discovered-hardware capacity policies) + L3b (the §24.1 fixed-capacity sweep) + L4a (`ulimit` shell command) + L4b (`sysinfo` limits query) are all landed. The L3b sweep converted: the userland-heap free-span table (grow-on-demand `SpanStore`), the `kernel/sec` supplementary-group ceiling (`CAP_RLIMIT_RAISE`-gated configurable capacity), the spawn fan-out `MAX_SPAWNS` (allocator-backed grow-on-demand page-table capacity on both production producers), the wasm32/riscv64/aarch64/x86_64 per-CPU handle bookkeeping (caller-provided `&'static`-slice capacities — `RiscvArchStorage<N>` / `Aarch64ArchStorage<N>` / `X86_64ArchStorage<N>` for the bare-metal ports, since the boxed approach is blocked by the allocator-free Stage-2 bins), the kernel stack arena (grows by chaining a fresh `FrameAllocator`-backed 2 MiB block on exhaustion and shrinks by returning idle chained blocks under a one-free-block grace, zeroed-on-free §4, fail-closed §2.9), and — on all three bare-metal ports — the per-arch secondary-bring-up bound (x86_64's `percpu::MAX_CPUS` is gone; its per-CPU GDT/IDT/IST arena, syscall-entry TLS, and AP bootstrap-stack pool are now caller-provided `PerCpuStorage<N>` / `SyscallTlsStorage<N>` / `ApStackPool<N>` storages, matching the aarch64/riscv64 `SecondaryStackPool<N>` + `PreemptStorage<N>` shape).** Implements `AGENTS.md` §24: resource *capacities*
must scale with discovered hardware (§18.1) and grow on demand, with
desktop-and-server-sensible defaults and a settable `ulimit`/`rlimit`-equivalent
— never a hard-wired `const` ceiling. This supersedes the fixed-arena follow-ups
previously noted against `plans/PI.md` (the stack arena is made growable here,
not patched in place). Security/format *bounds* on untrusted input stay fixed
and fail-closed (§24.4) — this work must not loosen them.

**Audited fixed-capacity sites to convert (the §24.1 sweep):**
- Kernel stack arena — `kernel/tairix-kernel/src/mem_map.rs`
  (`GUARD_ARENA_BYTES`/`GUARD_ARENA_ALIGN`, single 2 MiB block, single-shot
  carve) and `kernel/tairix-kernel/src/stack_arena.rs`
  (`StackArena` forward-only bump over a fixed `[base,end)`,
  `STACK_REGION_BYTES`). Sizing from the discovered RAM window is **done**
  (L3a — `stack_arena_bytes`). **Done** (L3b): `StackArena` now **grows** by
  chaining a fresh 2 MiB-aligned, independently block-split block on genuine
  exhaustion (`FrameArenaGrow` over the live `FrameAllocator`'s
  `alloc_order(9)`), bounded to the per-space identity window, preserving the
  §17.2 break-before-make and §4 guard-page invariants and failing closed to
  `BoxStack` only on physical exhaustion (§2.9); the aarch64, x86_64, and
  riscv64 production spawn seams all draw through it. **Done** (L3b —
  stack-arena *shrink*): the capacity
  falls as well as rises (§24.1 — grow *and* shrink, never a one-way ratchet):
    - **Per-block live-count accounting.** The arena is a linked list of blocks
      (boot-carved + each chained); each tracks the count of guarded regions
      currently handed out. `StackArena::free` (driven by `ArenaStack`'s `Drop`
      at task exit) locates the owning block by address range and
      checked-decrements its count; a foreign/misaligned address or an
      already-zero count is rejected without underflowing — fail closed (§2.9),
      surfaced as a typed `FreeOutcome`. A block whose count reaches zero is
      *idle*. The per-block `{ next, block_end, alloc_next, live, is_boot }`
      record lives in a reserved, identity-mapped header page at the block's own
      base — outside the guarded regions, accessed through the `BlockStore`
      seam (`IdentityBlockStore` in production; an in-memory map in the host
      tests) — so the block list is itself a §24.1 capacity (no second
      allocation, no fixed block cap).
    - **One-free-block grace (hysteresis).** Exactly one idle chained block
      stays resident: a chained block is returned to the allocator only when it
      goes idle *and* another idle chained block already exists, so an
      alloc/free oscillation across a block boundary reuses the retained idle
      block instead of repeatedly free→chain (amortised, no thrash, §2.16); an
      idle block is reset and reused before a fresh one is chained. Reclamation
      is at most one block return per `free` — never a spin/retry (§2.1) — under
      the existing `SpinLock` off the hot path.
    - **Boot block is never returned.** The boot-carved first block
      (`RegionKind::Reserved`, kernel-image-owned, not allocator frames) is
      never released; only `FrameArenaGrow`-chained blocks are reclaimed,
      through the symmetric `FrameArenaShrink` over `free_order(9)`.
    - **Secure / attack-safe.** A reclaimed block is fully zeroed before it
      returns to the `FrameAllocator` (§4 zero-on-free — a kthread kernel stack
      can hold spilled capability tokens/credentials); a block that cannot be
      safely scrubbed/returned is retained rather than released (fail closed,
      §2.17). The per-stack guard `split_block`/`unmap` was applied in the
      task's *own* root, torn down on exit, so reclaiming the block aliases no
      live mapping in the kernel's identity map. Internal kernel bookkeeping
      only — no new ambient authority (§4). The reclaim path (`ArenaStack` drop
      → the `Once`-published `'static FrameAllocator`) is freestanding-aarch64
      only; the accounting/grace/scrub logic is host-tested.
- Per-task stack size — `kernel/core/src/kthread.rs` `KTHREAD_STACK_BYTES`:
  **done** (L3a) — now a release-tuned policy value (32 KiB release / 64 KiB
  debug, §24.2).
- Per-arch CPU/hart handle bookkeeping — the dense-`CpuId`→hardware-id maps,
  host IPI ledgers, and per-core `CoreClass` tables in the arch handles.
  **wasm32 done** (`kernel/arch/wasm32/src/kernel_arch.rs`): `WasmArch`'s
  `cpu_to_worker`/`host_ipi_count` are now allocator-backed boxed slices sized
  to the discovered worker count (`worker_storage_len`, floor `boot_cpu+1`,
  §24.1/§24.2). **riscv64 done** (`kernel/arch/riscv64/src/kernel_arch.rs`):
  `RiscvArch` now borrows two `&'static [AtomicU64]` slices (the dense-`CpuId`
  → hart-id map, with the `u64::MAX` `NO_HARTID` sentinel, and the host IPI
  ledger) from a caller-provided `RiscvArchStorage<N>`, where the caller sizes
  `N` (a `static` for the allocator-free bins, a leaked allocation otherwise);
  the arch crate stays `alloc`-free, every accessor and the shootdown/IPI loops
  bound by the slice length, and the host suite + all nine riscv64 verticals
  construct through it. (riscv64's `MAX_HARTS` is now gone entirely — see the
  secondary-bring-up item below; `MAX_WORKERS` survives only as the wasm32
  `start_worker` worker-index bound.) **aarch64 done**
  (`kernel/arch/aarch64/src/kernel_arch.rs`): `Aarch64Arch` borrows three
  `&'static` slices — the dense-`CpuId` → `MPIDR_EL1` affinity map (`u64::MAX`
  `NO_MPIDR` sentinel, valid because MPIDR_EL1[63:40] are RES0), the host IPI
  ledger, and the per-core `CoreClass` table — from a caller-provided
  `Aarch64ArchStorage<N>`; `classify_from_fdt` finds the peak and classifies
  each core in two device-tree passes (the pure `hetcore::class_for_capacity`)
  with no fixed buffer, and `send_ipi`/every accessor bound by the slice length,
  so the handle imposes no `MAX_CPUS` ceiling (`MAX_CPUS` survives only for the
  `smp.s`/`preempt` secondary-bring-up bound). The production boot path supplies
  the handle over exactly-sized leaked slices (`Aarch64Arch::with_cpu_slices`)
  from the validated `/cpus` dense map (`tairix-kernel`'s host-tested
  `cpu_topology::order_cpus`); every aarch64 vertical supplies a right-sized
  `static`. **x86_64 done** (`kernel/arch/x86_64/src/kernel_arch.rs`):
  `X86_64Arch` borrows three `&'static` slices — the dense-`CpuId` → LAPIC-ID
  map (`&[AtomicU16]`, `u16::MAX` `NO_LAPIC` sentinel since a LAPIC id is a
  `u8`), the host IPI ledger, and the per-core `CoreClass` table — from a
  caller-provided `X86_64ArchStorage<N>`; the constructor populates the map from
  the caller's `&[Option<u8>]` MADT map with atomic stores, every accessor and
  `send_ipi` is bound by the slice length, and `shootdown_page` no longer fills a
  fixed `[u8; MAX_CPUS]` scratch buffer — it streams the other CPUs' LAPIC ids
  out of the borrowed map into `tlb_shootdown::shootdown`, now an `Iterator +
  Clone` consumer that walks them twice (count, then send). So the handle imposes
  no `MAX_CPUS` ceiling — and `percpu::MAX_CPUS` is now gone entirely (the
  per-CPU `percpu`/`syscall_entry` arenas and the AP stack pool are also
  caller-sized — the secondary-bring-up item below). Production `boot.rs`
  supplies a `static X86_64ArchStorage<1>`
  (single-CPU) and every x86_64 vertical a right-sized `static`. The boxed-slice
  approach the wasm32 port used is **blocked** on bare metal — `extern crate
  alloc` in a bare-metal arch crate forces `alloc` into the dependency graph of
  every freestanding bin that links it, so the deliberately allocator-free
  Stage-2 QEMU bins (e.g. `memory_isolation_qemu_aarch64`) would be forced to
  carry a 64 MiB bump heap they never use — hence the no-`alloc`
  caller-provided-`&'static` design.
- Per-arch secondary-bring-up bound — the assembly secondary-stack pools
  (`smp.s` `SECONDARY_MAX_*`) + per-CPU `static` storage (`preempt`/`percpu`),
  an SMP-bring-up redesign (an assembly `.bss` reserve cannot be discovery-sized,
  so the stack moves to a caller-provided runtime pool). **aarch64 done** (L3b):
  the `.bss` pool and the `MAX_CPUS` const are deleted; the secondary stack is a
  caller-sized `smp::SecondaryStackPool<N>` whose `register` publishes the base +
  per-core stride the `smp.s` trampoline computes each core's stack top from
  (`base + (cpuid+1)*stride`), and the per-CPU timer slots are a caller-sized
  `preempt::PreemptStorage<N>` published as `&'static [AtomicU64]` slices; both
  set-once and fail closed before registration (§2.9) — plus runtime-sized
  twins (`smp::register_secondary_stacks` over a leaked `[SecondaryStack]`,
  `preempt::register_preempt_slices`) the production boot sizes from the
  discovered `/cpus` count — with the §17.2/§4
  invariants preserved. **riscv64 done** (L3b): the `smp.s` `.equ
  SECONDARY_MAX_HARTS` + `.skip` pool and the `smp::MAX_HARTS` const are deleted;
  the secondary stack is a caller-sized `smp::SecondaryStackPool<N>` whose
  set-once `register` publishes the base + per-hart slice log2 size the `smp.s`
  trampoline computes each hart's stack top from (`base + (hartid+1) << shift` —
  a left shift, since the stub avoids the `M` multiply extension), and the
  per-hart timer slots are a caller-sized `preempt::PreemptStorage<N>` published
  as `&'static [AtomicU64]` slices; both fail closed before registration (§2.9),
  the per-stack 16 KiB size stays a fixed §24.4 bound, and the three riscv64
  SMP/timer verticals register a right-sized pool/storage. **x86_64 done**
  (L3b): `percpu::MAX_CPUS` is deleted; the per-CPU GDT/IDT/IST arena, the
  `syscall`-entry TLS, and the AP bootstrap-stack pool are now caller-provided
  `percpu::PerCpuStorage<N>` / `syscall_entry::SyscallTlsStorage<N>` /
  `smp::ApStackPool<N>` storages, each published through a set-once `register`
  and failing closed before it (every index out of range, no panic, §2.9). The
  Rust-mutated payloads are `UnsafeCell`-backed so the `static` lands in
  writable memory; unlike aarch64/riscv64 the AP reads its stack top from the
  per-AP boot slot the BSP stamps, so the Rust `start_secondary` computes it
  with no assembly `.bss` reserve. Production `tairix-kernel` registers
  `PerCpuStorage<1>` + `SyscallTlsStorage<1>`; the SMP verticals register a
  right-sized `PerCpuStorage<N>` (+ `ApStackPool<N>` for the multi-CPU ones,
  `scheduler_stress_qemu`'s old `MAX_CPUS <= percpu::MAX_CPUS` const-assert
  deleted).
- Process/identity capacities — spawn fan-out (`spawn_producer*.rs`
  `MAX_SPAWNS = 8`): convert to grow-or-limit-governed capacities. **Done**
  (L3b): the spawn fan-out itself is now allocator-backed and grows on demand —
  both production producers build a child's page tables over a boot-cached
  `kernel/mem` `FrameTableSource` (over the live `FrameAllocator`) instead of a
  fixed `[PageTablePool; 8]` `.bss` reserve, so there is no hard process cap and
  exhaustion fails closed with `Errno::NoSpace` (§2.9); the `MAX_SPANS = 256`
  userland heap span table (`lib/rt/src/heap.rs`) is now a grow-on-demand
  `SpanStore` (maps a fresh metadata page on exhaustion); and the `kernel/sec`
  supplementary-group ceiling — the former hard-wired
  `MAX_SUPPLEMENTARY_GROUPS = 32` const is now `DEFAULT_MAX_SUPPLEMENTARY_GROUPS`
  (the §24.2 default policy) plus a per-builder, `CAP_RLIMIT_RAISE`-gated
  configurable ceiling (`IdentityTableBuilder::with_supplementary_group_limit`);
  the supplementary-group store was already a growable `Vec`, and a candidate
  record can never raise the ceiling, so the §24.4 anti-DoS bound is preserved.
- ARXFS mount footprint (`drivers/filesystem/arxfs/src/{allocmap,allocator}.rs`)
  — **done** (the §24.1/§26.6 fix for the Raspberry Pi 4 eMMC2 boot OOM and for
  mount cost scaling with volume contents rather than volume size — mounting
  the shipped 128 MiB read-only `/System` volume used to cost 10,541 block
  reads and ~6 s of every boot). Free space is tracked by an on-disk **paged
  allocation map** (bitmap pages plus a per-page free-count summary, sealed
  under `BlockType::AllocMap`, updated in place rather than copy-on-written)
  instead of a walk of every tree, inode, and extent at mount. A mount
  **adopts** the map with a handful of reads when it authenticates at the
  committed transaction root's address and its clean/dirty stamp shows no
  update was left in flight; otherwise it rebuilds from the authoritative
  trees, so a crash between syncs costs one rebuild, never a correctness
  problem. Resident cost is a bounded LRU cache of at most
  `MAX_CACHED_MAP_BLOCKS` (64) region blocks per mounted volume,
  volume-independent, so several 100 TB+ volumes mount together on a 1 GiB
  machine — including a near-full one, which the previous working-set-sized
  approach did not cover. A **read-only mount holds no allocator state at
  all**: it cannot allocate, free, dedupe, or trim by construction, and reads
  only the superblock ring and the committed root, so mounting a read-only
  volume such as `/System` costs a handful of block reads rather than a
  volume-contents walk. The dedupe index is no longer pre-seeded at mount
  either; it warms from the writes that use it. The transient pending-discard
  queue stays capped at a fixed, volume-independent `MAX_PENDING_DISCARD`.
  Spec: `docs/src/filesystem/arxfs-spec.md` §4.
- Kernel heap arena — `lib/kalloc` `FreeListAllocator` / `HEAP_BYTES` — **done**
  (the §24.1 fix for the `stress --vm` kernel OOM panic): the heap was a fixed
  64 MiB `.bss` slab that, once exhausted, returned null from `GlobalAlloc` →
  `handle_alloc_error` → panic. It is now growable/shrinkable: the `.bss` region
  is a *bootstrap* only, and a late-installed `HeapSource` lets it draw fresh
  physically-contiguous frames from the live `FrameAllocator` on a miss and hand
  whole drained regions back. Production wiring is `kernel/core::kheap`
  (`register_global_heap` — an `AtomicPtr` slot each arch bin sets in
  `kernel_main` before `boot` — plus the frame-backed `FrameHeapSource` and
  `install_frame_heap_source`, called in `kernel_bringup` once the frame
  allocator and `arch.direct_phys_map()` exist), over a new
  `PhysMap::reverse(virt)->Option<PhysAddr>` (heap-shrink recovers a region's
  physical frame from its direct-map base). Growth draws the whole pool
  (kernel-internal), while a **user** commit is reserve-gated: `FrameAllocator`
  holds `reserve_frames = usable_frames / RESERVE_DIVISOR` (the divisor hoisted
  to `kernel/mem::frame`, shared with `pressure`), and `alloc_user` /
  `alloc_order_user` (which `LiveSpace::map_anonymous*` route through) refuse a
  draw that would drop the free pool to or below the reserve, so a greedy
  userland process fails closed with `Errno::OutOfMemory` before it can starve
  the kernel's ability to grow its heap. The frame allocator's `bitmap` is also
  rebased to the usable span (indexed from `base_frame`, like `nodes`/
  `blk_order`) so a high-based/§26.6 huge-address map costs bitmap metadata for
  its RAM, not its address extent. With the heap growable, the reclaimable cache
  budgets no longer size off `HEAP_BYTES` (now merely the bootstrap size):
  `kernel/core::memstats::cache_backing_bytes()` publishes discovered physical
  RAM (`usable_frames * PAGE_SIZE`, set in `kernel_bringup`), and
  `block_cache`/`transform_cache`/`volume_service`/`system_mount` derive their
  `CacheBudget::from_backing` from it.
- (Explicitly **out of scope / leave fixed**: the §22 RNG reserve
  `DEFAULT_RESERVE_BYTES`/`RANDOM_RESERVE_DEFAULT_BYTES` (charter-blessed), and
  all untrusted-input/format bounds — `lib/vt` `MAX_PARAMS`/`MAX_STRING`,
  `lib/fdt` `MAX_DEPTH`, `lib/svg` caps, ext4/fat32/arxfs format constants,
  path/name/command-line/config length caps. These are §24.4 defences.)

**Deliverables**
- L1 — **DONE.** `lib/abi` resource-limit ABI (`lib/abi/src/rlimit.rs`): closed
  versioned `LimitKind` enum (`AddressSpaceBytes`/`OpenStreams`/`Processes`/
  `StackBytes`, `COUNT`/`ALL`/`from_u32`/`name`), `ResourceLimit { soft, hard }`
  (`RLIMIT_INFINITY`, `intersect` never-widen, `encode`/`decode` fail-closed),
  the `rlimit_get` (#17) / `rlimit_set` (#18) syscalls, and `CAP_RLIMIT_RAISE`
  (id 20). Dispatcher arms route both to `SyscallHandlers::rlimit_get`/`_set`
  (default fail-closed `NotImplemented` until L2). `abi-sys` stubs
  (`tairix_sys_rlimit_get`/`_set`), `lib/rt` wrappers, generated `tairix_rlimit.h`,
  decoder added to the `fuzz_decode` harness; docs in
  `docs/src/architecture/resource-limits.md` + `syscalls.md`.
- L2 — **DONE.** Kernel enforcement in `kernel/core`. A per-task `LimitSet`
  (one `ResourceLimit` per `LimitKind`, default `LimitSet::DEFAULT` =
  unlimited until a later increment derives it from hardware) lives in the per-task
  `AddressSpaceRegistry` (`kernel/core/src/aspace.rs`) beside the stream
  table, withdrawn on exit. `rlimit_get`/`rlimit_set` are wired
  (`kernel/core/src/syscalls.rs`): both validate `kind`, copy through the
  `copy_to_user`/`copy_from_user` boundary, key off the kernel-trusted
  `caller.task_id`. `authorize_set` (`kernel/core/src/rlimit.rs`) refuses
  raising a hard bound above the current ceiling with `PermissionDenied`
  unless the caller holds `CAP_RLIMIT_RAISE` (§24.3); the audited `rlimit_set`
  logs the rejection (§19.4). A spawned child inherits the parent's set
  intersected against the default (`LimitSet::inherit`), never widened (§5.2).
- L3a — **DONE.** Discovered-hardware capacity policies for the kthread
  kernel stack. `tairix_kernel_core::KTHREAD_STACK_BYTES` is now release-tuned
  (32 KiB release / 64 KiB debug, both whole 4 KiB pages, §24.2). The guard
  arena is no longer a fixed 2 MiB block: `tairix_kernel::mem_map::
  stack_arena_bytes(ram_size)` sizes it from the discovered RAM window
  (≈1/64 of RAM, clamped `[2 MiB, 64 MiB]`, rounded down to a whole 2 MiB
  block so each guard page still becomes its own L3 leaf after
  `prepare_guard_arena`), threaded through `carve_guard_arena`/
  `build_memory_map`; a window too small to carve one block still degrades to
  the software-canary `BoxStack` (fail closed, §2.17). Host-tested (mem_map
  policy floor/scale/cap/whole-block/large-window + the existing region-tiling
  suite); the existing aarch64 guard verticals continue to prove the
  mechanism on the now-policy-sized arena. Docs in
  `docs/src/architecture/resource-limits.md`.
- L3b — **DONE.** The §24.1 fixed-capacity sweep is complete (no ABI change):
  the userland-heap free-span table in `lib/rt/src/heap.rs` is now a
  grow-on-demand `SpanStore` capacity — it maps a fresh metadata page when the
  table fills and fails closed only on genuine OOM, with a Vec-backed host
  store exercising the growth/fail-closed paths; and the `kernel/sec`
  supplementary-group ceiling is now `DEFAULT_MAX_SUPPLEMENTARY_GROUPS` (the
  §24.2 default policy) plus a per-builder, `CAP_RLIMIT_RAISE`-gated
  configurable ceiling (`IdentityTableBuilder::with_supplementary_group_limit`,
  fail-closed `PermissionDenied`, free to lower), backed by the already-growable
  `Vec` storage and 5 new host tests, with the §24.4 anti-DoS bound preserved
  (a record can never raise its own ceiling). The **spawn fan-out**
  (`MAX_SPAWNS`) is also converted: both production producers
  (`kernel/tairix-kernel/src/spawn_producer.rs` and `…_x86_64.rs`) now build a
  spawned child's page tables out of the kernel's live `FrameAllocator` through
  a boot-cached `kernel/mem` `FrameTableSource` (the W5b-3 source, backed by an
  identity `DirectPhysMap` so the port's `phys as *mut` table recovery stays
  valid), threaded through the new `KernelSyscallHandlers::with_page_table_frames`
  seam + `SpawnCtx::page_table_allocator`; the former fixed `[PageTablePool; 8]`
  `.bss` reserve and `MAX_SPAWNS` const are deleted, so the spawn capacity
  scales with discovered RAM and fails closed (`Errno::NoSpace`) only on genuine
  OOM (§2.9). `FrameTableSource.phys` was tightened to `&'static (dyn PhysMap +
  Sync)` so the one source can live in a `static Once`. The **riscv64 per-arch
  CPU/hart handle bookkeeping** is also converted: `RiscvArch` holds two
  caller-provided `&'static [AtomicU64]` slices via `RiscvArchStorage<N>`
  instead of `[T; MAX_HARTS]` arrays (no `alloc` in the arch crate; the
  unmapped-slot sentinel is `u64::MAX`), so the handle imposes no CPU ceiling
  and the allocator-free Stage-2 bins are untouched (no ABI change; the host
  suite + all nine riscv64 verticals construct through the backing). The
  **aarch64 per-arch CPU handle bookkeeping** is also converted: `Aarch64Arch`
  holds three caller-provided `&'static` slices (the `MPIDR_EL1` affinity map
  with the `u64::MAX` `NO_MPIDR` sentinel, the host IPI ledger, and the
  per-core `CoreClass` table) via `Aarch64ArchStorage<N>` instead of
  `[T; MAX_CPUS]` arrays; `hetcore` became the pure slice-scaling
  `class_for_capacity` and `classify_from_fdt` does two device-tree passes with
  no fixed buffer, so the handle imposes no CPU ceiling and the allocator-free
  Stage-2 bins are untouched (no ABI change; the host suite + all aarch64
  verticals + the production boot path construct through the backing). The
  **x86_64 per-arch CPU handle bookkeeping** is also converted: `X86_64Arch`
  holds three caller-provided `&'static` slices (the dense-`CpuId` → LAPIC-ID
  map `&[AtomicU16]` with the `u16::MAX` `NO_LAPIC` sentinel, the host IPI
  ledger, and the per-core `CoreClass` table) via `X86_64ArchStorage<N>` instead
  of `[T; MAX_CPUS]` arrays, and `shootdown_page` streams targets into a now
  `Iterator + Clone` `tlb_shootdown::shootdown` rather than a fixed
  `[u8; MAX_CPUS]` buffer, so the handle imposes no CPU ceiling and the
  allocator-free Stage-2 bins are untouched (no ABI change; the host suite + all
  eight x86_64 verticals + the production boot path construct through the
  backing). The **growable kernel stack arena** is also converted: `StackArena`
  grows *past* its policy size on genuine exhaustion by chaining a fresh,
  independently block-split 2 MiB block out of the live `FrameAllocator`
  (`FrameArenaGrow`, `alloc_order(9)`) bounded to the identity window, instead
  of failing over to `BoxStack`; both aarch64 production spawn seams
  (`init_spawn`, `spawn_producer`) draw through it, and it fails closed to the
  software-canary `BoxStack` only on genuine physical exhaustion (§2.9), with
  the §17.2 break-before-make and §4 guard-page invariants preserved. The
  **stack-arena shrink** is also converted: the arena is now a linked list of
  blocks (each with an intrusive identity-mapped `{ next, block_end,
  alloc_next, live, is_boot }` header accessed through the `BlockStore` seam —
  `IdentityBlockStore` in production, an in-memory map in the host tests — so
  the block list is itself a §24.1 capacity); `StackArena::free`, driven by
  `ArenaStack`'s `Drop` at task exit, locates the owning block by address
  range, checked-decrements its live count, and — under a one-free-block grace
  (hysteresis: release only the *second* idle chained block, reuse the resident
  spare otherwise — amortised, no thrash) — returns an idle chained block
  through the symmetric `FrameArenaShrink`/`free_order(9)`, never the
  boot-carved block. A reclaimed block is zeroed-on-free (§4); double-,
  foreign-, and misaligned-free fail closed without underflow (§2.9, typed
  `FreeOutcome`); a block that cannot be safely scrubbed/returned is retained
  (§2.17). The reclaim path threads the `'static FrameAllocator` through a
  `Once` published on the first runtime spawn. 22 host unit tests over a real
  `FrameAllocator` + an in-memory `BlockStore` cover the live-count, grace
  (release only on the second idle block; zero releases under boundary
  oscillation), idle-block reuse, fail-closed double/foreign/misaligned free,
  boot-block-never-released, and real-buffer zero-on-free scrub; the `Drop`
  reclaim seam + `free_order` return run on the aarch64 stack/spawn QEMU
  verticals. The **aarch64 per-arch secondary-bring-up bound** is also
  converted: the `smp.s` `.bss` `SECONDARY_MAX_CPUS` pool and the
  `kernel_arch::MAX_CPUS` const are deleted; the secondary stack is now a
  caller-sized `smp::SecondaryStackPool<N>` whose set-once `register` publishes
  the pool base + per-core stride that the `smp.s` trampoline computes each
  started core's stack top from (`base + (cpuid+1)*stride`, replacing the
  baked-in array index), and the per-CPU timer slots are a caller-sized
  `preempt::PreemptStorage<N>` published as `&'static [AtomicU64]` slices; both
  fail closed before registration (an unbacked `CPU_ON` / unrecorded tick is
  refused, §2.9), the per-stack 64 KiB size stays a fixed §24.4 bound, and the
  §17.2 break-before-make + §4 guard-page invariants hold. The four aarch64 SMP
  / timer verticals register a right-sized pool/storage and stay green on
  `-M virt`; production `tairix-kernel` registers runtime-sized backings from
  the discovered `/cpus` count and brings every discovered core online after
  `BootCompleted` (the `kernel_core` SMP phase: published secondary dispatch
  hand-off, audited PSCI `CPU_ON` per core, each secondary adopting the boot
  translation and joining the shared dispatch loop — proven end to end by the
  `-smp 4` `kernel-arch-boot-aarch64` vertical).
  The **riscv64** secondary-bring-up bound is now converted the same way (its
  `smp.s` `SECONDARY_MAX_HARTS` `.skip` pool and `smp::MAX_HARTS` const deleted
  in favour of a caller-sized `SecondaryStackPool<N>` / `PreemptStorage<N>`,
  the trampoline using a left shift to avoid the `M` multiply extension). The
  **x86_64** secondary-bring-up bound is now **also** converted, completing the
  sweep: `percpu::MAX_CPUS` is deleted and the three per-CPU `[T; MAX_CPUS]`
  statics it sized — the GDT/IDT/IST arena, the `syscall`-entry TLS, and the AP
  bootstrap-stack pool — are now caller-provided `percpu::PerCpuStorage<N>` /
  `syscall_entry::SyscallTlsStorage<N>` / `smp::ApStackPool<N>` storages, each
  with a set-once `register` and fail-closed accessors (every index out of
  range → `CpuIndexOutOfRange`/`CpuIdOutOfRange`, no panic, §2.9). The
  Rust-mutated payloads are `UnsafeCell`-backed so the `static` is writable;
  the AP reads its stack top from the per-AP boot slot the BSP stamps, so no
  assembly `.bss` reserve is involved. Production `tairix-kernel` registers
  `PerCpuStorage<1>` + `SyscallTlsStorage<1>`; the SMP verticals register a
  right-sized `PerCpuStorage<N>` + `ApStackPool<N>` (the `scheduler_stress_qemu`
  `MAX_CPUS <= percpu::MAX_CPUS` agreement const-assert is deleted), with the
  §17.2/§4 safety invariants preserved. **No per-arch secondary-bring-up bound
  remains.**
- L4a — **DONE.** The `ulimit` shell command in the default shell
  (`userland/shell/elsh`) over the L1 ABI. A new `tairix_elsh::LimitStore`
  seam (`get`/`set`, fail-closed `NullLimitStore` default + `Shell::with_limits`
  builder) threads through `Shell`/`BuiltinContext`; the `ulimit` builtin
  (`userland/shell/elsh/src/ulimit.rs`) parses `-a`/`-H`/`-S` + a canonical
  `LimitKind` name + a decimal/`unlimited` value, reports or imposes the
  process's own limits, preserves the unchanged bound on a one-sided set, and
  fails closed on an unknown flag/resource/value or a `soft > hard` request
  (never writing the store). The real `Run` binary installs `RtLimitStore`
  over `tairix_rt::rlimit_get`/`rlimit_set`; an in-memory `MemoryLimitStore`
  double drives 13 host tests. The `CAP_RLIMIT_RAISE` denial surfaces as a
  reported error (§2.9). Docs: `docs/src/architecture/resource-limits.md`
  ("The `ulimit` shell command") + the shell `README.md`.
- L4b — **DONE.** The `SysinfoQueryId::RESOURCE_LIMITS` (id 7) System
  Information query (§16.6) exposing the caller's own effective limits + live
  usage. `lib/abi/src/sysinfo.rs` gained the query id + spec row + the
  `ResourceLimitRecord` wire type (`kind`/`reserved`/`ResourceLimit`/`usage`,
  32-byte `#[repr(C)]`) and `RESOURCE_LIMITS_REPORT_LEN` (one record per
  `LimitKind`, discriminant order); the query is self-scoped, so — like
  `SELF_PROCESS_LIST` — it is ungated and unaudited (§16.6). `sysinfod` serves
  it through a new `SysinfoSource::resource_limits` seam (the per-task
  `LimitSet` + live usage); the `sysinfo` CLI gained the `limits`/`rlimits`
  command rendering one aligned row per resource (`unlimited` for
  `RLIMIT_INFINITY`), fail-closed on a wrong-length reply. C header
  regenerated (`tairix_resource_limit_record_t`, `TAIRIX_SYSINFO_QUERY_RESOURCE_LIMITS`,
  `tairix_rlimit.h` include); the new decoder is in the `fuzz_decode` harness
  (§19.6). **No syscall/hash change.** Docs:
  `docs/src/architecture/resource-limits.md`, `docs/src/abi/sysinfo.md`,
  `docs/src/userland/{sysinfod,utilities}.md` + the two READMEs.
- L5 — **done on the MMU ports** (staged as `plans/SPAWN.md` **SP11**;
  notes in `.junie/fix-fixed-stack-size.md`). The user stack is a
  **demand-grown** stack inside an 8 MiB reserved virtual span (guard page
  below the span preserved) with a 128 KiB eager commit: growth is
  fault-driven and **contiguous** (every page from the committed base down
  to the faulting page, so no unmapped hole can strand above the low-water
  mark) through the existing user-fault-resolver + `MemMap`-producer
  seams, bounded fail-closed by the settable `StackBytes` soft limit whose
  default (`tairix_kernel_core::DEFAULT_STACK_LIMIT_BYTES` in
  `LimitSet::DEFAULT`) is the one policy value the span is derived from.
  Proven end to end on all three MMU ports by the QEMU verticals
  (`stack_grow_program` + `stack_grow_qemu_{aarch64,riscv64,x86_64}`:
  transparent byte-verified growth, `rlimit_set`-lowered bound
  fault-kill, below-span guard kill) and the kernel/core host tests; the
  x86_64 twin composes the factored shared board bring-up
  (`x86_64::boot::bring_up_bsp`, SP11e) with the production hook in the
  production `DISPATCH_SLOT`. wasm32 linear memory is the honest n/a.

**Tests**
- Default policy yields a workable capacity on both a tiny and a large
  discovered-hardware fixture; stack arena **grows** past its first block under
  many-spawn load and still faults on guard-page overrun; physical exhaustion
  fails closed (no panic, §2.9).
- soft/hard bound semantics, `CAP_RLIMIT_RAISE` gate on raising a hard bound,
  and inheritance/intersection across spawn + delegation (§7); `ulimit`
  round-trips through the ABI; fuzz the new `lib/abi` rlimit decoder (§19.6).

**Docs**
- `docs/src/architecture/resource-limits.md` (the §24 policy + the `ulimit`
  model); rustdoc on every new public item; update `docs/src/abi/` for the new
  syscalls/capability.

## CURSES — text-mode / TUI stack (`plans/CURSES.md`)

**Status: complete (C1–C5).** The shared text-mode vocabulary and curses
stack. Layering: all crates depend on `lib/*` only and live outside
`userland/gui/*`, so a headless image links them (§17.3/§17.4). One
escape-sequence definition end to end (§2.2).

- C1 — `lib/vt` (`tairix-vt`): the canonical ANSI/VT/xterm vocabulary
  (control bytes, colour models, SGR, `Cell`, `Op`) with an emitter and a
  streaming parser over the same tables (emit→parse identity); the parser is
  total and fail-closed (bounded params/buffers, drops malformed input). Fuzz
  harness `fuzz_vt`.
- C2 — `userland/apps/terminal` refactored onto `lib/vt` as a *consumer* (no
  private parser); xterm-256color-class `Grid` (scroll region, alt screen,
  saved cursor, OSC title) with honest `TERM`.
- C3 — `lib/termcap` (`tairix-termcap`): compiled-in `TERM`→capability database
  (no terminfo file, §16.1) with the closed versioned `TermType` set; every
  record expressed in `lib/vt` terms; `from_term` fails closed to `Dumb`.
- C4 — `lib/curses` (`tairix-curses`): client `Window`/pad draw model, a
  minimal-diff capability-aware renderer (truecolour→256→16→mono downgrade),
  and an input decoder (keys/mouse/paste) over `lib/vt`; `Screen<T: Tty>`
  I/O-injected driver. Fuzz harness `fuzz_curses_input`. Added the key/mouse/
  paste ops to `lib/vt`.
- C5 — curses completeness (wide/UTF-8 cells, colour-pair alloc, `getch`/
  timeout input; panels deferred until a consumer needs them) + the first
  consumer `userland/apps/top` (live process TUI over `lib/procinfo` +
  `lib/curses`).
- C6 — `lib/fbcon` (`tairix-fbcon`): the shared, architecture-neutral
  framebuffer text-console engine. A full ANSI/VT/xterm-256color terminal
  (`Geometry` / `TextConsole` / `DirtyBand`, palette, glyph blit over
  `lib/font`, scroll-up-at-bottom, and the **alternate screen** — `CSI ?
  1049 h`/`l`) that renders the shared `lib/vt` `Op` stream onto a 32-bit
  scan-out surface over a **retained `tairix_vt::Cell` grid**, so every arch
  port (`x86_64`/`aarch64`/`riscv64`/`wasm32`) drives its display console
  through one definition rather than re-deriving the emulation per target
  (§2.2/§2.20/§2.21). The grid lets a full-screen program (`top`, an editor)
  enter a cleared alternate screen and, on exit, have the primary screen it
  covered restored exactly — the xterm-family contract. The two cell grids
  (primary + alternate) are **borrowed** `&mut [Cell]` the caller owns, so
  the engine stays allocator-free: a freestanding boot console with no global
  allocator supplies a `static`, an allocator-having caller leaks a heap
  buffer sized to the discovered geometry (`Geometry::cell_count`, §24.1 — no
  fixed ceiling). Depends on `tairix-vt`/`tairix-font`
  `default-features = false`; host-unit-tested (incl. alt-screen restore).
  The aarch64 port's `video.rs` consumes it two-phase: pre-MMU discovery
  records the surface and clears it; post-MMU (once the heap is usable)
  `boot` leaks the grids and calls `video::attach_console`, which builds the
  console and activates the screen — the per-CPU `Aarch64ArchStorage`
  storage-ownership pattern applied to the cell grids. The `lib/vt` `alloc`
  feature is optional (default-on) so the emitter's `Vec`-returning `encode*`
  helpers stay available to allocator-having consumers while the parser path
  is taken allocator-free by `fbcon` and the console.
- The conventional text grid. The console atlas is authored at an 8×16
  cell (`tairix_fontface::ATLAS_EM_PX = 14`), the character cell PC text
  consoles have used since VGA, so a text screen is the expected `width / 8`
  × `height / 16`: 80×30 at 640×480, 128×48 at 1024×768, 160×64 at
  1280×1024, 240×67 at 1080p. `Geometry::for_display` draws that cell one
  atlas pixel per screen pixel, so a denser panel holds more cells rather
  than magnified ones: replicating each pixel into a square block throws
  away the grid fitting and anti-aliased edges below, which is exactly the
  blocky type a magnified console shows.
  Glyphs are grid-fitted before they are filled. The committed face carries
  no TrueType hinting bytecode, so at an 8-pixel cell its stems land between
  pixels and antialias into two grey columns: unfitted, under a tenth of the
  atlas's ink reached full coverage and nearly half sat at mid-grey.
  `lib/fontface`'s `gridfit` snaps each stroke onto whole pixels (never
  narrower than one, so a hairline darkens rather than vanishes) and holds
  every letter's baseline, x-height and cap height to shared rows, taking
  full-coverage ink to over a third — past FreeType's autofitter on the same
  face and size — and leaving grey only on curves and diagonals, which must
  stay antialiased. The cell path also scales columns so the face's uniform
  advance lands on a whole number of cells instead of overhanging one — a
  full-width fallback face is placed across its two cells, not squeezed into
  one; the `fontd` proportional path fits rows only, so ink stays under the
  advance a client laid out with.
  Four rules keep the fitting faithful to the letter, each pinned by a
  regression test: a stroke is bounded by two edges the outline travels in
  *opposite* directions, so the near sides of a `j`'s hook and its stem are
  never read as one stroke and the stem snapped out of existence; an edge must
  be flat within a fraction of a pixel and not merely shallow, so a `w`'s arms
  stay diagonals instead of being sheared onto a column; every stroke of a
  glyph is placed by the one shift its first stroke sets, so an `m`'s three
  evenly-spaced stems stay evenly spaced rather than rounding to 1, 4, 6; and
  an edge keeps the runs it covers rather than their span, since a side is
  often several — an `m`'s top is the left stem's flat and two arch crowns, a
  `g`'s is its bowl's crown and its ear — so the ink test is asked where both
  sides really have material and not in the valley between, which otherwise
  left the arch a third of a pixel thick and took the `g`'s top and ear with
  it.
  Box Drawing (U+2500–U+257F) and Block Elements (U+2580–U+259F) are emitted
  as pixel-exact geometry by `lib/fontface`'s `lineart`, not rasterised from
  the face: at an 8-pixel cell the face's hairlines fall between pixels and
  antialias, so borders rendered grey and a filled `█` region showed a seam at
  every cell edge. A double rule is derived as the outline of the region its
  arms sweep, so all twenty-nine junctions agree without a per-glyph table.
  It lives in the engine rather than the generator because both sources of a
  character grid's glyphs draw from it: the compiled-in atlas and `fontd`.
  Every script the family ships is compiled in. The console runs in the
  kernel and cannot ask `fontd` for a glyph, so a face left out of the atlas
  is a script no console could ever draw — a `man` page in it, a login
  prompt, a panic. The generator reads the `mono` family's `FontFamily`
  manifest through the same `tools/xtask` store reader that plants
  `/System/Fonts`, so the console's faces and the shipped store's faces are
  one list and adding a face to the family is the whole change. At the 8×16
  cell the four faces are 23,602 cells in 1.6 MB, against 3.6 MB for the
  20,209 cells the old 15×28 atlas carried.
  The graphical terminal draws by the same rules. `fontd` renders a
  *monospace* family's glyph into its character cell — the cell being the
  family's own uniform advance, the outline grid-fitted to it, `left = 0` so
  the client blits at the cell origin — and substitutes the same `lineart`
  geometry for the two tiling ranges, so a border in `terminal.app` is the
  picture the console draws rather than an antialiased approximation of it.
  Over printable ASCII at the 13-pixel size a terminal opens at, fully-opaque
  ink rises from 13% to 34%. A proportional family keeps the tight-to-the-ink
  raster and its own bearings, which is what per-glyph layout needs. The
  synthesised geometry is computed per request rather than retained (it is
  arithmetic over one cell, and holding it would evict a real glyph), and the
  retained rasters' cache key carries the cell count, since how many cells a
  scalar spans is a property of the scalar and a face may map a wide and a
  narrow one onto one glyph.
  - **Open follow-up — text-console screenshots.** The `README.md` gallery's
    text-console images (`docs/screenshots/boot-filesystem-unlock.png`,
    `booted-and-logged-in.png`, `system-monitor.png`) still show the former
    68×27 grid at the old 15×28 cell. Regenerating them needs an interactive
    graphical QEMU session driven to each state, so it is its own task; the
    desktop and terminal images are unaffected (the terminal's grid and font
    sizing did not change).

## SHELL prerequisites (`.junie/PREREQUISITES2.md`)

Staged prerequisites the shell (`plans/SHELL.md`) depends on so it stays a pure
interpreter reaching effects through injected seams, with no second parser or
I/O vocabulary. See `.junie/PREREQUISITES2.md` for the full P0–P6 status.

- P6 — glob/pattern matching as a shared library: **done.** `lib/glob`
  (`tairix-glob`) is the one first-party filename-glob matcher (`*`, `?`,
  `[...]` bracket expressions with ranges/negation, `\` escaping), so the
  shell's filename generation and completion import it rather than embedding a
  private matcher (§2.2). It is `no_std`+`alloc`, `#![forbid(unsafe_code)]`,
  fail-closed on a malformed pattern, and matches with the backtracking-free
  two-pointer algorithm; pattern length, token count, and bracket size are
  fixed security bounds (§24.4). Scope decision: **glob, not a full regex
  engine** — globs are what the shell expands and match in bounded time; a
  regex dialect (with catastrophic backtracking) would be a separate engine if
  a consumer ever needs one. Unit tests, rustdoc, and the `fuzz_glob` harness
  ship with it.
- Filename completion as a shared library: **done.** `lib/complete`
  (`tairix-complete`) is the one path-candidate policy interactive completion
  applies — the directory-part/leaf split, the dotfile rule, the leaf-prefix
  filter, and the longest-common-prefix Tab discipline — imported by the
  shell's Tab completion and `fstree`'s destination prompts (§2.2, extracted
  when the second consumer arrived with `.junie/fstree-next-plan.md` S10).
  Presentation stays per consumer (the shell escapes inserts and merges its
  command/resource candidate classes; `fstree` inserts verbatim). It is
  `no_std`+`alloc`, `#![forbid(unsafe_code)]`, read-only by construction
  (the injected `DirLister` seam only lists), and fail-closed (a refused
  listing completes to nothing). Unit tests, rustdoc, and a docs page ship
  with it.
- P4 (path parser) — shared filesystem path-spelling parser as a `lib/*` crate:
  **done.** `lib/path` (`tairix-path`) is the one definition of how a TAIRiX
  path string is lexed and normalised into a typed `Root` + components, so the
  shell (`cd`, prompt display, word/tilde expansion, completion) and every other
  consumer import it rather than embedding a second path parser (§2.2). It
  parses the forms with present consumers — synthetic view (`/path`), alias
  shorthand (`Alias:/path`), the expanded internal `alias::Name/path`, and
  relative paths — and is `no_std`+`alloc`, `#![forbid(unsafe_code)]`,
  fail-closed and bounded (path/component/count/alias sizes are fixed security
  bounds, §24.4), with `..` unable to escape a rooted path and `:` reserved as a
  structural delimiter (so a rendered path always re-parses). Resource-reference
  shapes (`namespace:selector`) are declined with `NotAPath` (owned by the
  future ALIAS grammar, P5) and the durable/administrative resolvers
  (`id::`/`fs::`/`<driver>::`/`dev::`/`net::`) with `UnsupportedResolver` — they
  have no consumer yet, so inventing them here would be speculative interface
  (§2.3/§2.4). Unit tests, rustdoc, a docs page, and the `fuzz_path`
  round-trip harness ship with it. The **binding storage-namespace spec** is
  landed: `docs/src/filesystem/drives.md` turns `plans/DRIVES.md` into the
  forest-of-named-roots model (alias/`id::` canonical identity, `/` a generated
  view), with the §16.1 charter amendment (the four names become synthetic view
  bindings backed by first-class aliases). The **descriptor-producing
  open-a-path ABI** is landed (`fs_open` + the `fs_close`/`fs_read`/`fs_write`/
  `fs_readdir`/`fs_stat`/`fs_truncate`/`fs_sync`/`fs_mkdir`/`fs_unlink`/
  `fs_rename` family, `CAP_FS_ACCESS`-gated, fail-closed), and **machine-alias
  resolution** is wired at the single kernel path-resolution entry point:
  `Alias:/path` / `alias::Name/path` resolve for the four machine aliases,
  which are the canonical roots the `/` view projects as `/<Name>`
  (`kernel/core::fs::resolve_machine_alias`, derived from the one `ROOT_TEMPLATE`
  so the view and alias namespace cannot drift, §2.2), then authorised
  identically to the projected view path; an unpublished alias fails closed
  with `NotFound`. **Durable `id::` resolution is landed**
  (`plans/DEVICES.md` D3a): `lib/path` gained `Root::VolumeId` in place and
  the kernel volume forest (`kernel/core::fs::volumes`, installed via
  `BootInfo::with_volumes`) resolves a published volume's stable identity —
  the ARXFS per-volume UUID, published by the boot mount/unlock paths with
  audited `fs.root.publish.{allow,deny}` events — at the same entry point,
  fail-closed for an unpublished id. **Runtime attach/unpublish is landed**
  (`plans/DEVICES.md` D3b): the `volume_attach`/`volume_detach` syscalls
  mount a hot-pluggable volume under `/Storage/<name>` and publish/withdraw
  its `id::` root through the same forest. **Automount, catalog
  enumeration, and the mount-policy identity map are landed**
  (`plans/DEVICES.md` D3c/D3d). **Still open under P4** (tracked, not
  stubbed — not a shell blocker): alias policy for runtime volumes and the
  `fs::` resolver `Root` variant, at which point machine aliases rebind to
  independent `id::` roots without changing the resolver contract.
  Remaining prerequisites (P5) are tracked in `.junie/PREREQUISITES2.md`.
- P5 (reference parser) — shared resource-reference parser as a `lib/*` crate:
  **done.** `lib/resref` (`tairix-resref`) is the one definition of how a TAIRiX
  resource reference is lexed and validated into a typed `ResourceRef`, so the
  shell (redirection targets, command arguments, completion, typed shell values)
  and the resolver services import it rather than embedding a second reference
  parser (§2.2). It parses the `plans/ALIAS.md` grammar
  `namespace:selector[@guard][::facet][?params]` — including the `disk:@7K2M`
  fingerprint shorthand and the `disk:?…` query-only form — and defines the
  closed namespace registry (`KnownNamespace`) once. It is `no_std`+`alloc`,
  `#![forbid(unsafe_code)]`, fail-closed and bounded (ref/namespace/segment/
  guard/facet/param sizes and counts are fixed security bounds, §24.4), with the
  reserved delimiters `: / @ :: ? ,` never literal inside the part they delimit
  (so a rendered reference always re-parses). It is **spelling only**: it never
  resolves a namespace, opens a resource, verifies a fingerprint, or checks a
  capability — those resolver-level errors (`UnknownNamespace`,
  `CapabilityDenied`, `IdentityMismatch`, …) belong to the resolver services
  (§16.3 of `plans/ALIAS.md`). A string with no `:` is `NotAReference` (a
  filesystem path, owned by `lib/path`). Unit tests, rustdoc, a docs page, and
  the `fuzz_resref` round-trip harness ship with it.
- P5 (resolver + descriptor path, first namespace) — **done for `sys:`.** The
  `resource_open` `abi-v1` call (no. 67) is the resource-reference analogue of
  `fs_open`: it copies a reference in, resolves it through
  `kernel/core::resource` over `lib/resref` (never a second parser), and mints
  a **resource-backed** descriptor from the *same* per-process number space as
  `fs_open` (so a resource fd cannot collide with a file fd). The resolver
  serves `sys:random` (the CSPRNG reserve `random_get` draws from, read-only)
  and `sys:null` (empty source / discard sink), fail-closed: authorisation is
  per namespace (both unprivileged), so `resource_open` carries no blanket
  dispatcher capability, and a malformed/unknown/unwired/unserviceable
  reference mints no descriptor. Resource fds read/write with `fs_read`/
  `fs_write` and close with `fs_close` — whose capability check moved from the
  dispatcher into the handler so a path-backed descriptor still requires
  `CAP_FS_ACCESS` while a resource read never demands it; `fs_readdir`/
  `fs_stat`/`fs_truncate`/`fs_sync` fail closed on a resource fd. Ships with
  the `tairix_rt::resource_open` + `File::open_resource` wrappers, the
  `tairix_sys_resource_open` C stub + regenerated header, and host/proptest/fuzz
  coverage. The kernel resolver serves only *kernel-owned* backings; it fails
  `info:`/`stats:` closed (resolving those in the kernel would bypass the
  `sysinfod` broker's per-principal scoping — a `plans/ALIAS.md` §2 non-goal).
- P5 (userspace `info:`/`stats:` resolver) — **done for the shipped sysinfo
  queries.** `lib/procinfo::resolve` maps a parsed `resref` `ResourceRef` onto a
  `SysinfoQueryId`, issues it through the same client seam `ps`/`sysinfo` use,
  and returns the `plans/ALIAS.md` §14 response envelope
  (`lib/procinfo::resinfo::ResourceResponse` — an `InfoValue` or a `Metric` with
  producer, authorization, timestamp, and per-metric kind/unit/window/reset),
  never free-form text and never a second reference parser (§2.2). It serves
  `info:system/{hostname,kernel,machine-id,boot-time}` (from `SYSTEM_IDENTITY`,
  machine-id sensitive; `boot-time` from the ungated `UPTIME` reply as a public
  stable fact), `stats:uptime` (from `UPTIME`, boot-reset counter), and
  `stats:mem/{used,available,total,kernel-heap,user-resident}` (from
  `KERNEL_MEMORY_STATS`, gated on `CAP_SYSINFO_KERNEL`, gauges); it fails closed
  on an unknown selector, a
  guard/facet/query where none is served, a capability denial, or a malformed
  reply. Ships with host tests and the `fuzz_resinfo` harness (hostile
  references + hostile broker replies). **Still open under P5** (tracked, not
  stubbed): grow the userspace resolver in place as more sysinfo queries land,
  and wire the *kernel-owned* device namespaces into `kernel/core::resource`
  beside `sys:` via the device manager as their consumers appear — neither
  changes the `resource_open` contract.

## CCOMPAT — C-callable `abi-v1` (full `lib/abi` header, syscall stubs, crt0)

Staged build plan: `plans/CCOMPAT.md` (binding). Makes the **whole** of
`lib/abi` (every `#[repr(C)]` type, constant, enum discriminant — not just
syscalls) callable from non-Rust programs (C first), so `lib/abi` is a public
third-party developer surface (§9). The C header under `include/` is a
*generated* view of `lib/abi` (never hand-maintained, §2.2), drift-guarded by
`cargo xtask c-header` in `ci`. The stub runtime is **not** a privileged
bypass — every capability/input check stays kernel-side (§5.4) and C binaries
obey the `rxe`/`abi-v1` hardening invariants (PIE, W^X, CFI tag) identically.
This adds the curated `/System/Libraries/` *System runtime / C ABI* class
(§16.4), dynamically linked. Native Tier-1 only (x86_64/aarch64/riscv64); no
wasm32 (no trap instruction).

**Status: complete (CC1–CC5).** See `plans/CCOMPAT.md` for deliverables/tests.
- CC1 — full `lib/abi` C header surface: `cargo xtask c-header` emits one
  `include/tairix/` header per module + the umbrella `tairix_abi.h`, all values
  read from `lib/abi`, with a completeness test pinning every `#[repr(C)]`
  type's size/align + a tree-wide drift guard.
- CC2 — `lib/abi-sys`: the export-name-pinned `tairix_sys_*` stub runtime
  marshalling into the canonical register layout and issuing the real
  `syscall`/`svc`/`ecall` (the §1 asm carve-out), panic-free, no added
  authority; host tests + a QEMU trap round-trip per native target.
- CC3 — `lib/crt0`: per-arch `_start` trampoline + allocation-free
  `build_c_runtime` (lays out C `argv`/`envp`, installs the §19.2 stack canary,
  calls `main`, routes return through `tairix_sys_exit`); with kernel-side
  `build_process_image` + the `EnterUser` HAL primitive (riscv64 `sret`,
  aarch64 EL0 `eret`, x86_64 `iretq`) and `spawn_and_enter`
  (`CAP_PROC_SPAWN`-gated, audited). QEMU-proven spawn round-trips on all three.
- CC4 — loader/bundle integration: `rxe` gained a needed-shared-library table;
  `appmgr` validates `Run` against the kernel CFI tag and resolves needed libs
  only from `/System/Libraries/` or the bundle's `Libraries/` (fail closed).
- CC5 — end-to-end real C program built with the audited pinned `tools/cc`
  (clang + ld.lld, §12) → `rxe` → spawned under QEMU on all three native
  targets, exercising `Time64`/ipc/sysinfo + `cap_query`/`clock_get`; the new
  decoders are fuzzed with a regression corpus.

## APPS — application structure, command help, and command resolution (`plans/APPS.md`)

**Status: in progress.** Specifies how every program — from a graphical app
down to a single-binary utility like `ps`/`top`/`cat` — is organised as a
`<Name>.app` bundle (§16.5), how its command-line help is authored and served,
and how the shell resolves a typed command name to a runnable bundle. Binding
design lives in `plans/APPS.md`. The once-open maintainer question was decided
as the **merge**: the bundle's `Documentation/` entry was retired and renamed
to `Help/`, the single internationalised structured-Markdown tree that feeds
the TAIRiX `man` command, CLI short `-h`/`-?` help, and any GUI help viewer
(§16.5 amended; rationale in "Charter Amendments").

**Landed:** (1) `BundleEntry::Help` replaced `Documentation` in place (§2.13)
with every caller/fixture updated, `docs/src/abi/appinfo.md` refreshed, and
the C header regenerated (`TAIRIX_BUNDLE_ENTRY_HELP`); (2) `lib/help`
(`tairix-help`) — the one help engine: validated `Locale`/`DocumentName`
spellings, an injected capability-scoped `HelpSource` read seam, the
deterministic exact → same-language → canonical `en-US/` fallback (served locale
reported for `man`'s `stdinfo` record), a bounded fail-closed
structured-Markdown parser (fixed section model, typed `HelpError`,
fence-aware section walk), and `render_short`/`render_full` over `lib/vt`
(widths from `lib/curses`); unit-tested, fuzz-hardened (`fuzz_help` in
`cargo xtask fuzz`), documented (`docs/src/lib/help.md`); (3) **shell
command resolution** (`plans/APPS.md` §8–§9) with the §16.2/§16.8
`/System/Commands/` and `/System/Applications/` program stores (charter
amended, rationale in "Charter Amendments"): the store paths and bundle
suffix are defined once in `lib/abi` (`SYSTEM_COMMAND_STORE`,
`SYSTEM_APPLICATION_STORE`, `HOME_COMMAND_STORE_DIR`,
`HOME_APPLICATION_STORE_DIR`, `BUNDLE_SUFFIX`), every OS program is
registered as a name-matched bundle in the store its own manifest kind
selects (`kernel/tairix-kernel/src/program_manifests.rs`, host drift-tested
against the shared definitions), and the shell resolves a command word
through the pure candidate policy `tairix_cmdres::resolution_candidates`
(hoisted into the shared `lib/cmdres` crate, whose `bundle_candidates` view
`man`'s bundle lookup and every program's own `-h` import; explicit paths
bypass the search; `.app` names the bundle; bare words search the fixed
non-overridable prefix — both system stores then the user's own two — then the
alias-aware `:`-split `PATH`, empty entries skipped) — the `Run` binary's
host attempts candidates in order (spawn `NotFound` ⇒ next, any other
refusal final), and the interpreter maps failures onto `127` "command not
found" / `126` not-executable. The `spawn` ABI now carries a child's
argument vector and environment (`plans/SPAWN.md` SP8), and the shell
passes the typed words plus its exported variables to every launched
program — the prerequisite `man <cmd>` and the locale variable needed.
Proven end to end by the session-ceiling QEMU vertical typing the bare
word `ps` and the argument-carrying `ps --bogus`; (4) **the `man.app`
command app** (`plans/APPS.md` §7): `userland/apps/man` resolves a word
over the shared `tairix_cmdres::bundle_candidates` order (first existing
bundle; `NotFound` moves on, any other refusal final) and then, for a bare
word no candidate matched, over the bounded breadth-first recursive search
of `/Apps` and `<HOME>/Apps` (`tairix_cmdres::search_roots` over
`tairix_abi::INSTALLED_APP_STORE`; never descends into a `.app`; an exhausted
directory budget is reported, never silently "not found" — `plans/APPS.md`
§7), renders through
`lib/help`, reads the now-named `LANG` locale variable (`plans/APPS.md`
§5), `PATH`, and `HOME` from the inherited environment, pages on a
geometry-attested console and streams otherwise, and emits the
`help.locale_fallback` `stdinfo` `context` record on a locale fallback.
It is registered as `/System/Commands/man.app/Run` (manifest: console pair +
`CAP_FS_ACCESS`), ships its own thirteen-locale `Help/` tree (authored on disk
in the bundle, discovered by `tools/syshelp`, and planted onto the
read-only `/System` volume's `Apps/` store by `tools/mkimage` and the QEMU
image fixture — the system volume skeleton carries `Apps/` per §16.2), and
the session-ceiling vertical
types `man man` end to end. The generic argv/stderr-line helpers were
hoisted into `lib/rt` (`args`, `io::write_stderr_line`) and the `-errno`
decode into `tairix_abi::Errno::from_syscall`, each now one definition.

`ls` implements GNU `-s`/`--size` — the allocated size per entry in
1024-byte blocks (with `-h` scaling) plus the `total` line every directory
listing now prints under `-l`/`-s` (GNU parity) — backed by honest
allocation plumbing: `NodeInfo`/`DelegatedInfo`/`FileStat` carry an
`allocated`-bytes field filled from each format's real tracking (ext4
`i_blocks`, huge-file aware — and the osd2 `file_acl` high-half mis-read
fixed with a regression test; FAT32 cluster-chain walk; ARXFS extent-tree
sum; memfs length), with the C header regenerated.

Every store-registered command app (`cat`, `groupadd`, `ls`, `man`, `ps`,
`top`, `sysinfo`, `useradd`, `users`, `elsh`) ships its thirteen-locale `Help/`
tree and honours the
`-h`/`-?` short-help convention through the one shared `lib/help` render
(`own_short_help` + the `rt`-feature `BundleHelp` own-bundle source);
per-locale switch-drift unit tests pin each tree's `OPTIONS` to its
parser, and the tools that gained filesystem reach for the help read
request `CAP_FS_ACCESS` (the man/ls precedent).

`cargo xtask help-lint` (plans/APPS.md deliverable 5) is live: the one
shared judgement `tairix_help::lint_help_trees` (`lib/help`'s host-only
`lint` feature, also run by the `tools/syshelp` aggregator tests) gates
every discovered `Help/` tree in `cargo xtask ci`/`ci-long` — spellings,
structural bounds, canonical `en-US/` presence, required-locale completeness
(the standing `tairix_help::REQUIRED_LOCALES` set — `fr-FR`/`de-DE`/`es-ES`/
`uk-UA`/`it-IT`/`pt-PT`/`cy-GB`/`zh-CN`/`ja-JP`/`ko-KR`/`ar-SA`/`he-IL` —
the one definition every per-app switch pin also imports), no
translation-only documents,
cross-locale `OPTIONS` switch-key drift, the content-policy word screen
(plus the CJK substring screen for the unsegmented languages),
and per-command coverage over the `AppInfo.toml` discovery walk.

The `plans/APPS.md` §12.1 Stage B registrations are under way: `cp`, `mv`,
and `rm` are full self-contained store bundles (Run host with the
stderr+stdin `y`/`Y` prompt seam, `AppInfo.toml` requesting the console
pair + `CAP_FS_ACCESS`, thirteen-locale `Help/` trees with switch-drift pins,
`-h`/`-?`/`--help` over the shared `own_short_help`/`BundleHelp` render).
They are store-only — the §18.6 boot floor never grows, so the kernel
inventory drift test pins their `AppInfo.toml` directly and no
`spawn_layout`/`spawn_paths` row exists for them. Landing `mv` added the
missing `EXDEV` equivalent in place: the dedicated `Errno::CrossVolume` /
`VfsError::CrossVolume` a cross-mount `fs_rename` is refused with
(regression-tested in `kernel/core`'s delegate tests; C header
regenerated), so `mv`'s copy-then-remove fallback triggers on exactly
that condition and no other. `useradd` and `groupadd` are likewise
registered store-only bundles over the existing `users_admin` syscall
(`CAP_CONSOLE_WRITE` + `CAP_USER_ADMIN` + `CAP_FS_ACCESS`; no
console-read — they never prompt), each with a host-tested production
client behind injected channel/entropy seams; the shared
account-authoring policy (`tairix_users::{DEFAULT_SHELL, default_home,
next_id}`) was hoisted into `lib/users` and the `users` session +
`tools/mkimage` deduplicated onto it, and `useradd` creates the account
with an unusable random password record (the GNU `!`-field equivalent —
a password is set afterwards via the `users` tool), the
session-baseline ceiling, and the shared shell/home defaults.

The `plans/APPS.md` §12.1 Stage C coreutils build-out is under way:
`true`, `false`, `yes`, `basename`, `dirname`, `mkdir`, `rmdir`, `head`,
`wc`, `tee`, `seq`, and `whoami` are
full self-contained store bundles (store-only — the §18.6 boot floor
never grows, so the inventory drift test pins their `AppInfo.toml`
directly; console-write + `CAP_FS_ACCESS` — plus console-read for the
stdin-reading `head`/`wc`/`tee` — thirteen-locale `Help/` trees with
switch-drift pins, complete GNU surfaces). `head` carries the full GNU
surface (elide counts with the multiplier-suffix alphabet, `-q`/`-v`/
`-z`, the obsolete `-COUNT[bkm][lqvz]` first-argument form) streaming
in constant memory; `wc` carries `-c`/`-m`/`-l`/`-w`/`-L`, `--total`
(argmatch prefixes), `--files0-from`, and the exact GNU column-width
rule (summed regular-file sizes, 7-column non-regular minimum,
unpadded single-input/single-count, files0, and total-only forms),
measuring `-L` through the one `tairix_vt::char_width` definition and
decoding UTF-8 incrementally across chunks; `tee` carries `-a`, `-p`,
and `--output-error[=MODE]` with the GNU failure discipline (its
documented divergences and the staged `-i` are recorded in
`plans/APPS.md` §12.1); `seq` carries `-f`/`-s`/`-w` with GNU operand
scanning, the spelling-derived default precision/width, a C-locale
`%e`/`%f`/`%g`/`%a` format engine, the exact decimal fast path
(arbitrary-size integer runs, `inf` LAST), and the extra-number
rounding rule — its one documented divergence: the float path computes
in `f64`, not glibc's `long double`; `whoami` reads the caller's uid
from the kernel-attested origin record (the ungated `self_origin`
syscall) and pairs it with a name through the ungated `USER_DIRECTORY`
sysinfod query over the shared `tairix_procinfo` account-directory walk
(the `top` USER-column helper — one definition), reporting the GNU
`cannot find name for user ID` diagnostic for an unlisted uid and a
service error — never a fabricated "missing name" — for a failed walk.
`basename`/`dirname` are
purely lexical and treat a `Name:/` alias root the way POSIX treats `/`
through the path grammar's own exported rule
(`tairix_path::alias_root_len` — one definition, no second path
parser); `false`'s served short help exits `0` per the §4 short-help
convention (documented divergence from GNU `false --help`). `echo` and
`pwd` stay `elsh` builtins (the shell resolves builtins first, and
`pwd` needs the shell's cwd state). Landing `mkdir`/`rmdir` evolved
`abi-v1` in place (unfrozen, the `mv`/`CrossVolume` precedent): the
dedicated `Errno::NotADirectory`/`Errno::NotEmpty` codes (with
`VfsError::{AlreadyExists, NotADirectory, NotEmpty}` now mapping
precisely instead of collapsing onto `OutOfRange`) and a validated
`UnlinkFlags` word on `fs_unlink` whose `DIRECTORY` bit is the atomic
`rmdir(2)`/`unlinkat(AT_REMOVEDIR)` posture, decided by the filesystem
under its own lock — `rmdir` carries no stat/remove race, and `rm`'s
own directory removals now pass it too. Both tools' `-p` walks share
the one ancestor-spelling rule (`tairix_path::Path::prefix`); `mkdir`'s
GNU `-m` remains staged — its kernel prerequisite (`fs_set_mode`,
syscall 74, landed with the registered `chmod` bundle and `fstree`'s
mode editor; owner-only, `CAP_FS_ACCESS`-gated, audited) now exists,
and the flag lands with its own tests in its own change, never
stubbed. The C header carries the new errnos
plus `TAIRIX_UNLINK_FLAG_DIRECTORY` and the previously-unpublished
`TAIRIX_OPEN_FLAG_*` bits (regenerated, drift-guarded).

The `edit` full-screen curses text editor (`userland/apps/edit`) is a
registered store bundle in the QuickBasic/MS-DOS-editor shape: an
F10-driven `File`/`Search` menu bar, a status line, insert/overwrite
editing, wrap-around find, and whole-file load/save through an injected
`Fs` seam over the kernel-authorised `fs_*` syscalls (console pair +
`CAP_FS_ACCESS`, the interactive file-tool request). Decoding is
fail-closed (UTF-8 text only, 16 MiB validation bound, lone-CR/binary
refused); tab→space expansion and CRLF→LF conversion are announced on
the status line, and final-newline presence round-trips. It draws
through `lib/curses` (`Screen::colored_attributes` — the colour helper
hoisted from `top` so both apps share one definition) and ships the
thirteen-locale `Help/` tree with the switch-drift pin.

**The `vim` editor (`plans/VIM.md`).** The modal text editor is a
registered store-only command bundle (`userland/apps/vim`,
`vim.app`, console pair + `CAP_FS_ACCESS`, thirteen-locale `Help/`): the
vim core — normal/insert/replace/visual/command-line modes, counts,
registers, `d c y` over motions and text objects, undo/redo and
dot-repeat, `/`/`?` search over a bounded pattern subset, and the ex
core (`:w :q :e :r :s` with ranges) — behind host-testable
`FileIo`/`Tty` seams. Landing it added the modal-editor input events
every curses consumer now shares: `Event::Esc` (bare-`ESC`-at-read-end
resolution in `lib/vt`) and `Event::Ctrl` in `lib/curses`. The staged
road to full vim (wrap/CJK display, marks/macros/visual-block, the
full pattern engine, `:g` and ex parity, buffers/windows/tabs,
swap/vimrc/viminfo, syntax/vimscript) lives in `plans/VIM.md`.

**Remaining** (staged in `plans/APPS.md` §13): the Stage B remainder
(`chmod`/`chown`/`getcap`/`setcap`/`mount` blocked on their kernel
syscalls — fs mode/owner set, per-inode capability get/set, mount — each
registers in the change that lands its syscall); the Stage C remainder
(the rest of the first batch — `printf`, `tail`, `env`,
`sleep`, `date`, `id` — then the text tools, in
further batches); `Help/` trees for future
command apps as each becomes a registered store bundle; and wider `stdinfo`
adoption in command apps as future behaviour warrants it (advisory-only,
§20 — the live records are `man`'s locale-fallback, `ls`'s hidden-entries
omission, and the shared `proc.self_scope_only` omission `ps` and
`sysinfo processes` both emit via `tairix_procinfo::emit_self_scope_omission`;
the other registered commands have nothing non-obvious to add today).
A cross-app *shared-library* bundle stays declined —
§16.4 refuses cross-bundle library references — so a resource-only bundle may
share **data**, not dynamically-linked code.

**Self-contained bundles — migrate off the kernel-baked spawn registry
(§16.5 amended 2026-07-04, §16.2 services-are-apps amended 2026-07-04).**
Every discovered bundle (command apps *and* the
`login`/`devmgr`/`sysinfod` services) ships complete on the read-only
`/System` volume — its signed `AppInfo` + `Run` rxe beside its `Help/`
tree — and on the aarch64 production boot the `spawn` syscall loads and
verifies those on-disk bundles through the shared `tairix_appload` gate
(increment 4 below); no kernel-baked rxe copy remains there. The x86_64
and riscv64 ports still carry the embedded registry as their
explicitly-justified §18.6 boot floor until increment 5 lands their
storage bring-up. Maintainer decision 2026-07-04: no staged
compatibility — the full correct end state, in dependency order, each
increment landing complete and green:

1. **Canonical bundle content hash** — one framing definition in `lib/abi`
   (`appinfo`) of the digest over every file the `AppInfo` signature covers
   (everything in the bundle except `AppInfo` itself, path-sorted,
   length-framed), shared by the build-time composer and every
   `BundleStore::content_hash` implementation (§2.2) — **done**:
   `tairix_abi::digest_bundle_contents` + `BundleFileDigest`, host-tested.
2. **Per-bundle manifest source + discovery + signing** — each app/service
   crate authors its own `AppInfo.toml` (id, name, version, kind
   command/service, capability request); a discovery walk over the userland
   crate roots (never a per-bundle list, §2.2/§16.5) finds them; the shared
   host composer (`tairix-itest-harness::app_image`) composes and signs the
   wire `AppInfo` under `SYSTEM_APP_SIGNING_SEED` (`build_support.rs`, a
   trust domain distinct from the driver-signing seed) — **done**:
   `AppInfo.toml` in every program crate,
   `app_image::{discover_app_manifests, compose_signed_appinfo}` with a
   fail-closed line-based grammar, unit tests verifying a composed
   manifest against the exact `lib/crypto` verification contract (and that
   a tampered capability body breaks it), and a `tairix-kernel` drift test
   pinning every `AppInfo.toml` against `program_manifests.rs` until the
   registry dies. Landing this surfaced and fixed a real defect: the
   `AppInfo` signature covered only the fixed header, leaving the
   capability-id body swappable behind a valid signature — the signed
   message is now the header prefix ‖ body, enforced in `appmgr` with a
   regression test.
3. **Plant the bundles** — **done**: `tools/xtask`'s `image_apps` pipeline
   (over the shared `pie_build` PIE cross-compile recipe `image_drivers`
   also uses) discovers every `AppInfo.toml`, builds each program's `Run`
   rxe, and composes its signed `AppInfo` whose content hash covers the
   exact `Run` + `Help/` bytes the planters lay down; `tools/mkimage`
   (`build_rpi_image`/`build_system_partition`) and the QEMU whole-disk
   fixture (`build_image_with_contents`/`build_image_with_apps`, /System
   grown to 32 MiB) plant each bundle's `AppInfo` + `Run` beside its
   `Help/` (`/System/Commands/<cmd>.app/`,
   `/System/Applications/<app>.app/`, `/System/Services/<name>.app/`) —
   the Pi image and every `EncryptedRootDisk` vertical carry complete
   self-contained bundles, composed once per xtask process and memoised.
   The service paths are bundle-form everywhere: PID 1 `init`'s startup
   config and the kernel registry name
   `/System/Services/{login,devmgr,sysinfod}.app/Run`, spelled from the
   shared `tairix_abi::SYSTEM_SERVICE_STORE` (drift-tested).
4. **Kernel disk-backed spawn** — **done.** The bundle-verification engine
   lives in the shared `lib/appload` crate the kernel links (§17.4;
   `AppLoader` pipeline, `BundleStore`/`Verifier` seams, `LoadedApp`, the
   `11000..12000` audit-event range), `userland/system/appmgr` re-exporting
   it as its user-space consumer. The `spawn` syscall resolves an absolute
   `…/<Name>.app/Run` path as an on-disk store bundle
   (`kernel/core/src/appspawn.rs`): read through the secured VFS under the
   **caller's** kernel-attested identity, verified against the build's
   embedded app trust anchor (`SYSTEM_APP_SIGNER_PUBKEY`, a trust domain
   distinct from the driver anchor), content-hash + ABI/syscall-hash
   checked, the child's capability request derived from the on-disk
   manifest; a spawn racing the boot mount parks on the `AppStore`
   readiness latch, which the `/System` mount install resolves on every
   outcome. The embedded command-app/service rows are gone from the aarch64
   production boot. Verification runs **once per boot** per read-only
   store bundle: the accepted `LoadedApp` is cached in the `AppStore`
   (LRU under a discovered-RAM-fraction byte budget,
   `APP_CACHE_RAM_DIVISOR`) and a later launch serves the cached image
   after re-authorising the caller's read of `Run` through the secured
   VFS — command-launch latency is a designed hot path, and re-verifying
   an immutable bundle per launch (≈1.2 s per command under QEMU TCG)
   was the defect this closes; writable-volume bundles (`/Apps`) are
   never cached. Load-bearing invariant: the **system principal**
   (`uid 0`) resolves to the capability-less bootstrap identity (`gid 0`,
   no supplementary groups) in the secured-VFS group resolution whenever
   the identity table is absent or holds no `uid 0` record — PID 1 must
   spawn the boot services off `/System` *before* the encrypted root is
   unlocked, and an installer image's table never defines `uid 0`; every
   per-inode/mount check still applies and non-zero uids stay strictly
   fail-closed (`LateIdentity::resolve_groups`, regression-tested in
   `kernel/core/src/fs/mounted_tests.rs`).
5. **Per-port storage floor, then delete the registry** — x86_64 and riscv64
   gain their bootstrap-floor disk, image layout, and read-only `/System`
   mount (their staged `tools/mkimage` builders, §12), after which
   `SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s (all but PID 1 `init`),
   `spawn_paths.rs`, and `program_manifests.rs` are deleted (§2.14). Until
   that lands, the embedded registry is those ports' explicitly-justified
   §18.6 boot floor — the only reason it still exists. The x86_64 side is
   staged as `plans/ARCHSUPPORT.md` increments A1–A2 (riscv64 rides the
   same shared work).

---

## USB — modular USB stack + device hot-removal (`plans/USB.md`)

**Status: done (U1–U5 landed; live attach/detach acceptance on Pi 4 metal is
the operator's step — QEMU models no Pi USB).** The stack is the three
independent layers the §17 modularity contracts require: a bus driver emits
the controller node, the user-space host-controller driver (HCD,
`drivers/bus/usb/xhci`) owns one controller and serves the bus-agnostic URB
transport IPC (`lib/abi/src/usb_urb.rs`), and per-device class drivers
(`drivers/input/usb_kbd`, …) bind emitted per-interface nodes and submit URBs
— no bus↔class hardwiring (§2.20, §17.4). Hot-removal is structural: the HCD
watches ports event-driven and calls `hw_remove_node`; `devmgr` unloads the
bound driver through the kernel driver-unload mechanism
(`StoreRequest::Unload`), and re-plug re-enumerates a fresh device and
autoloads. The URB transport serves control and interrupt transfers; bulk is
the DEVICES-plan extension. See `plans/USB.md` for the binding design and
per-increment guarantees.

---

## DEVICES — device inventory commands + USB mass storage (`plans/DEVICES.md`)

**Status: done (DEVICE1 V1–V3; DEVICE2 D1–D4). Live path: Pi 4 metal acceptance (no emulated fixture publishes USB nodes).**
DEVICE1 adds the `lspci` and `lsusb` system command apps: they render the
discovered PCI/USB nodes from the existing `CAP_SYSINFO_HW`-gated
hardware-tree query, naming devices through the `lib/devids` lookup crate
whose data is vetted, provenance-pinned snapshots of the public PCI
(`pci-ids.ucw.cz`) and USB (`linux-usb.org/usb.ids`) ID databases, imported
and malicious-content-filtered by a developer-run `cargo xtask devids
--fetch` (never a build-time network fetch, §19.3) with a CI drift gate.
V1 (done) delivered `lib/devids` — the one definition of the snapshot
grammar, the strict fail-closed vetting filter (whole-file grammar
validation, UTF-8 with no control bytes so no terminal-escape injection,
exact-width lowercase-hex ids in emitted scopes, per-scope duplicate
rejection, fixed size/name/entry bounds), and the compact-table codec
(sorted 12-byte records over an interned strings blob; alloc-free O(log n)
`vendor`/`device`/`class`/`subclass`/`prog_if` lookups over a fully
validated view) — plus the `cargo xtask devids` pipeline: the committed
snapshots under `lib/devids/assets/` carry provenance headers (upstream
URL/version/date, fetch date, raw SHA-256, transport/encoding statements,
licence), `--write` regenerates the compact tables (each written into its
consuming command bundle's `Resources/` —
`userland/apps/lspci/Resources/pci.ids.bin`,
`userland/apps/lsusb/Resources/usb.ids.bin`), the no-flag verify is
a `ci` static gate, and `--fetch` imports (pci.ids over TLS; usb.ids over
upstream's canonical HTTP URL — upstream offers no valid TLS endpoint — with
integrity from the pinned SHA-256 + reviewed diff, and any stray ISO-8859-1
byte deterministically promoted to UTF-8 and recorded). The `fuzz_devids`
harness (vetting parser + table decoder) is registered with
`cargo xtask fuzz`. PCI subsystem entries and the auxiliary usb.ids sections
are validated but deliberately not encoded: no consumer renders them (the
hardware tree records no subsystem ids). V2 (done) delivered the `lspci`
command app (`userland/apps/lspci`): the hardware tree via the shared
paged `tairix_procinfo::hwtree::fetch_tree` client, names via the bundled
`Resources/pci.ids.bin` table (read at runtime through the VFS, covered by
the signed `AppInfo` content hash), the `pciutils` option surface over what
the model carries (`-n`/`-nn`/`-v`/`-t`/`-d`/`-s`; addresses are stable
hardware-tree node ids, `-k` withheld until driver-binding records exist),
thirteen-locale `Help/`, and generic build-side bundle-`Resources/`
discovery (`tairix_syshelp::RESOURCE_FILES` → AppInfo digest + both
planters). V3 (done) delivered the `lsusb` command app
(`userland/apps/lsusb`): the same posture over the USB view — the
`usbutils` option surface (`-v` interface class/subclass/protocol names,
`-t` controller→interface topology, `-d`/`-s` filters with the `usbutils`
`[[<bus>]:][<devnum>]` grammar), bus/device numbers derived from the
hardware tree's stable node ids (controller parent id / interface node
id, the documented divergence), names via the bundled
`Resources/usb.ids.bin`, the `usb.names_unresolved` fd-3 advisory,
thirteen-locale `Help/`, and a `lsusb --help` step in the SP10b pipeline
vertical — with the shared hardware-tree walk (decode, stable bus order,
depth, ancestor-keep, class labels) hoisted into
`tairix_procinfo::hwtree` so `lspci` and `lsusb` render through one
definition. DEVICE2: D1 (done) added bulk transfers to the URB transport;
D2 (done) delivered the `drivers/storage/usb_msd` Bulk-Only-Transport class
driver — a pure user-space class driver that derives its interface and bulk
endpoint pair from the device's own configuration descriptor, drives the
fail-closed BOT/SCSI engine (with the spec's stall/retry/Mass-Storage-Reset
recovery, landed alongside the URB seam's no-data control-OUT), and serves
each ready LUN as a `tairix_abi::blkio` block-service endpoint + 32 KiB
shared window behind an emitted Storage-class node
(`tairix,usb-msd-lun`), write-protect enforced driver-side; host-proven
over scripted doubles, Pi 4 metal acceptance for the live path (QEMU
models no Pi USB). D3a (done) landed the durable `id::` roots: `lib/path`
`Root::VolumeId` (canonical hyphenated lowercase UUID spelling only,
fail-closed), the kernel volume forest
(`kernel/core::fs::volumes::VolumeForest`, threaded
`BootInfo::with_volumes` like the other late-installed seams) resolving an
`id::<volume-id>/path` at the single kernel path-resolution entry point to
the view location the published volume's root backs (authorised by the
secured VFS identically — never a policy bypass), and boot publication of
both boot volumes' ARXFS UUIDs with audited `fs.root.publish.{allow,deny}`
events. D3b (done) landed the runtime half: the `volume_attach` /
`volume_detach` syscalls (`CAP_FS_MOUNT` + per-resource grant checks,
audited), the kernel blkio-client `Block` over a served endpoint + shared
window (counted kernel hold on the window's frames), the runtime-mutable
mount table with per-mount permission templates, `Arc`-shared driver
registration with `unregister`, forest `unpublish`, and the
`RuntimeVolumeService` mounting ARXFS/ext4/FAT32 under `/Storage/<name>`
with full unwind and the drives.md hotplug audit events — ext4/FAT32
gained `FilesystemStats` + volume identity along the way (and the ext4
formatter's nil-`s_uuid` defect was fixed: the caller now mints the UUID).
D3c (done) landed the automount policy: `drivers/storage/volmgr`, a
per-node autoloaded policy driver (the D3b grant model gates the blkio
endpoint and `volume_attach` behind the matched node's own grants, so the
per-node instance — not a singleton watcher — is the least-privilege,
zero-new-kernel-surface design) that probes a whole-device filesystem
signature else the GPT/MBR partitions **by content** through the new
`lib/fsprobe` crate (the one home of the ARXFS/ext4/FAT32
signature/label/identity definitions, imported by the fs drivers
themselves), derives the deterministic catalog name (sanitised label →
`<fstype><n>` → identity-fingerprint suffix on collision), and issues the
audited `volume_attach` per volume, exiting 0 run-to-completion — the
`mbr::encode` silently-drops-`Other`-partitions defect was fixed en route
with a regression test. It now recognises a **RAID array member** at each
extent's block 0 (via `fsprobe::probe_raid_member`) *before* any filesystem
signature and refuses to attach a bare member — mounting one raw mirror copy
would diverge the array or serve stale data (§26.5); the on-disk RAID
array-superblock format and reassembly were hoisted into the new `lib/raidmeta`
crate so the RAID composition engines (`lib/raid`) and this probe
share one definition (§2.2) without a `drivers/*`→`drivers/*` edge (§17.4).
The blkio client itself (`RemoteBlock` over the `BlkCall` transport seam,
plus its production `RtBlkCall` async transport) was likewise hoisted out of
this crate into the new `lib/blkclient` crate and grew a real write path and
an explicit, named access stance (`connect_read_write` / `connect_read_only`),
so the RAID array composer can share the identical client and wire
discipline; `drivers/storage/volmgr` keeps its narrow authority through the
read-only constructor (`plans/FIX-IO.md` IO6).
D3d (done) landed the user-facing mount policy:
the well-known `storage` group (`tairix_users::STORAGE_GROUP`, resolved by
name from the loaded group registry at root unlock into the set-once
`volume_policy::LATE_STORAGE_GID` cell), the `GroupMappedFs` identity map
an ownerless FAT32 attach mounts under (system-owned, group `0o775`/`0o664`,
`set_security` refused; owner-model volumes untouched, no gid → fail-closed
restrictive default), and `Storage:` catalog enumeration (`MountTable::
direct_children` merged into `fs_readdir`, deduplicated, structural
entries). D4a (done) landed the surprise-removal state machine: the
`kernel/core::fs::retained` uncommitted-write journal (`RetainedWrites` +
`JournaledBlock` with watermark device flushes over the `Block::flush`
durability primitive, budget/pressure-bounded, wiped on release), the
`callreg::teardown_owned_by` endpoint-vanish observer seam (wake first,
then notify), and the volume-service transitions — clean unplug retracts
(event 4176); dirty enters unavailable-dirty with the set retained (4177);
abandoned retention enters unavailable-lost (4178); an unavailable
volume's registry slot is re-pointed at a fail-closed stand-in so even
cached reads report `DeviceFault`, and a plain detach of it is refused —
plus two defects fixed en route with regression tests (`VfsError::Io` now
maps to `Errno::DeviceFault`, and `Fat32::format` takes a caller-minted
non-zero BPB serial so two fresh FAT32 volumes no longer share one
identity). D4b (done) landed force-unmount: the `VolumeDetachRequest`
force byte, the kernel force-discard path (event 4179 — a healthy volume
still commits cleanly under force; only an impossible commit discards,
with the loss audited), the `MountRecord` availability byte + stable
volume identity (so the mount listing marks
`unavailable-dirty`/`unavailable-lost` and the tooling resolves names to
detach identities), and the new `unmount` command-app bundle
(`userland/apps/unmount`, `unmount [-f|--force] NAME`). D4c (done)
landed verified re-insert: an attach whose `lib/fsprobe`-probed identity
matches an unavailable volume is recovered in place — the journal's
dual-acceptance mutation-evidence shadow (seeded from the per-format
`fsprobe::evidence_len` window: `ARXFS` superblock ring / ext4
superblock / FAT32 boot+FSInfo) is re-read and, when every evidence
block matches its committed-or-latest copy, the retained writes replay
and commit (event 4185) and the volume returns to full service under
its original mount and `id::` root; any doubt fails closed to the
read-only `MountAvailability::RecoveryConflict` state with the retained
set kept for the audited force-discard (event 4186). `Fat32::format`
now lays out the FSInfo sector and backup boot pair (a
format-conformance defect the evidence window exposed). See
`plans/DEVICES.md` for the binding design and per-increment guarantees.

---

## NETWORK — full IPv4 + IPv6 networking (`plans/NETWORK.md`)

**Status: N1–N3b done; N3c–N9 planned.** N1 (done) landed the `lib/net`
protocol-engine foundation: the dual-stack address vocabulary
(`core::net` types + RFC 4007 `Ipv6Scope`/`ScopedIpv6Addr` zone rules),
the one RFC 1071 checksum (incremental accumulator + v4/v6
pseudo-header seeds), the Ethernet/ARP/IPv4/ICMP codecs migrated in from
the deleted interim `userland/net/icmp` responder (IPv4 parse now
verifies the header checksum), and the bounded, provider-agnostic RFC 4861
neighbour cache (`NeighborTable`: pure `now`-driven state machine,
action-channel `advance`, one-shot `next_deadline`, LRU-of-resolved
eviction failing closed) — fuzzed via `fuzz_net_eth`/`fuzz_net_addr`
and documented in `docs/src/lib/net.md`. N2 (done) landed the complete
dual-stack network layer in `lib/net`: options-tolerant/strict-emit
IPv4 with emit-side fragmentation, the bounded IPv6 extension-header
walk with the RFC 8200 dispositions, one shared ICMP/ICMPv6 machinery
(echo, errors, the RFC 4443 §2.4(e) generation gate + token-bucket
rate limiter), RFC 4861 Neighbour Discovery codecs driving the one
neighbour table, budgeted fail-closed fragment reassembly (overlap ⇒
drop, per-source/global budgets, oldest-first eviction), and routing
(one generic LPM trie for v4/v6, default-router list, RFC 6724 source
selection, RFC 8201 path-MTU cache) — property-tested and fuzzed via
`fuzz_net_ipv4`/`fuzz_net_ipv6`/`fuzz_net_icmp`/`fuzz_net_nd`. N3 is
staged as three tree-green sub-increments (N3a/N3b/N3c). N3a (done)
landed the per-interface RFC 4862 address engine (`iface`: static
v4/v6, SLAAC with DAD, RS scheduling, the §5.5.3(e) two-hour rule,
injected interface identifier), the dual-stack host engine (`stack`:
one per-interface `Stack` — frames + explicit `now` in, bounded frames
+ typed `StackEvent`s out; event-driven `advance`/`next_deadline`;
owned-address-only ARP/NS answering, bounded pending-transmit
resolution, budgeted reassembly, gated + rate-limited ICMP errors,
bounded RA application, first-hop-validated redirects, echo in/out),
and the driver seam's facts half (`Net::device_facts` returning the
fail-closed-validated `DeviceFacts` with the closed `NetOffloads`
vocabulary; `virtio_net` serves it) — end-to-end host tests (two
stacks ping each other over v4 and v6) and the `fuzz_net_stack`
harness. N3b (done) landed the `netstack` service process
(`userland/net/netstack`, service account uid 14): the alias-named
interface table over per-interface `Stack`s, the frame-ring pump, the
wait-set event loop with one-shot deadlines, the audited
`CAP_NET_ADMIN` admin surface on the reserved `NETSTACK_ENDPOINT`,
the broker facts/state reads behind `CAP_SYSINFO_INTROSPECT` narrowed
by `sysinfod` (`NET_INTERFACE_FACTS`/`NET_INTERFACE_STATE`) and
resolved by `lib/procinfo` (`info:net/…`, the first `state:net/…`
namespace), and the seam's transport half evolved in place — `Net`
frame I/O is now the shared-memory frame-ring transport
(`tairix_abi::driver::net_ring`), served by `virtio_net`. N3c (done)
replaced the interim icmp responder's QEMU coverage with the
`tests/integration/netstack_*` verticals on all three covered arches —
the netstack engine's ring pump drives a live virtio-net device against
the harness-side `netpeer` link peer (the same `lib/net` `Stack` over a
QEMU dgram unix-socket netdev): ping in/out over v4 and v6, neighbours
resolved both ways, the peer's own verdict required — and **deleted**
`userland/net/icmp` (§2.13/§2.14). The
remaining increments
deliver the complete dual-stack user-space network
stack above the link-layer driver seam: one pure, host-testable,
fuzzed protocol engine (`lib/net` — Ethernet, ARP/ND over one neighbour
contract, IPv4 + IPv6 as peers, ICMP/ICMPv6, IGMP/MLD multicast
membership, UDP, full RFC 9293 TCP with SACK and pluggable congestion
control), driven by the `userland/net/netstack` service (the §19.5
minimum-capability parser process; event-driven, never polling), serving
a versioned capability-gated socket ABI (`lib/abi/src/net.rs`; `CAP_NET`
and `CAP_NET_BIND_PRIVILEGED` land with their enforcement points, joining
the live `CAP_NET_ADMIN`) over kernel-brokered endpoints, and completing
the NIC seam's negotiated offload vocabulary over the landed
shared-memory frame rings (`virtio_net` serves it first; the software
path stays the conformance oracle). DoS resistance is designed in: SYN
cookies, RFC 5961
challenge ACKs, CSPRNG ISNs/ports/IDs, budgeted fail-closed reassembly
and neighbour caches, per-principal §24.3 accounting. Interfaces are
observable through `info:net`/`state:net`/`stats:net` sysinfo queries and
configured declaratively: the fail-closed
`/System/Settings/Network/network.conf` store (`lib/netconfig`, a
`lib/sysconfig`-shaped sibling engine) plus stack-wide `configure net.*`
keys, with interface bonding (`active-backup` failover + flow-hashed
`balance`) as a stack-composed virtual interface over unmodified NIC
drivers (N9). See `plans/NETWORK.md` for the binding design and
per-increment guarantees. Name resolution is a consumer of that socket
ABI, staged in `plans/DNS.md`: the pure RFC 1035 / RFC 5452 stub-resolver
engine in `lib/net::dns`, the userland client `lib/resolver` (drives the
engine over a `netsock-v1` UDP socket, reading the active recursive-server
set from the ungated `NET_RESOLVER_SERVERS` sysinfo query), and the `host`
command app as its first consumer; the live 3-arch DNS QEMU verticals
remain.

---

## STRESSTEST — stress testing + live kernel monitoring (`plans/STRESSTEST.md`)

**Status: ST1–ST5 done; ST6 planned.** Makes TAIRiX's behaviour under
load observable and provokable with first-party tools. ST1 (done) exports the
counters the kernel keeps — the `kernel/mem` pressure gauge, reclaim ledger,
and `ramzip` accounting, plus per-CPU load — as four audited
`CAP_SYSINFO_KERNEL` sysinfo queries (`MEMORY_PRESSURE`, `RECLAIM_STATS`,
`RAMZIP_STATS`, `CPU_LOAD`, ids 13–16) with matching `info:cpu/*` /
`stats:mem/*` / `stats:cpu/*` resolver selectors and `sysinfo` CLI
subcommands; the export rendezvous is the arch-neutral
`kernel/core::memstats::MEM_STATS` registry (one system pressure gauge,
per-cache `Arc<CacheAccounting>` ledgers, and the process-global `ramzip`
tier's stats feed installed by the boot path — reporting a truthful idle
all-zero tier until one is populated), and `SchedulerPolicy` gained the
`cpu_switches`/`queue_depth`
observations both policies implement under conformance cover. The bound
kernel IRQ table is exported the same way: `IRQ_LIST` (id 19,
`CAP_SYSINFO_HW`, audited — line ownership is cross-principal surface
topology like the hardware tree/seat inventory) returns one `IrqRecord`
per bound line (id, owning driver task, monotonic since-boot fire count,
quarantine flag), read through the shared `tairix_procinfo::for_each_irq`
walk by the `sysinfo irq` CLI subcommand, the `info:irq/<line>/owner` /
`state:irq/<line>/quarantined` / `stats:irq[/<line>]/count` resolver
selectors, and `sysmon`'s interrupt-lines panel — one definition, no
divergence; the count already exists (the runaway-quarantine
accounting), so serving it costs nothing steady-state. ST2 (done)
landed the memory-pinning API behind `plans/SWAPSWAPSWAP.md` §5's "pinned"
eligibility class: `mem_pin` (92, `CAP_MEM_PIN`, audited) / `mem_unpin`
(93, ungated, audited) mark the caller's whole anonymous memory exempt from
the compressed tier — the per-task registry's `is_pinned` mark is the
classifier's `pinned`-attribute source, never inherited across spawn,
cleared on exit — bounded by `LimitKind::PinnedMemoryBytes` over the pinned
footprint (mapped address space + committed stack) at `mem_pin`,
`mem_map`/`file_map`, and stack growth, with a per-boot derived default
(installed RAM / 8) installed as the registry default limit set,
`CAP_MEM_PIN` in the administrative ceiling, and the aggregate observable
as `RamzipStats.pinned_bytes` / `stats:mem/pinned` (proved end to end by
the `mem_pin_qemu_aarch64` vertical). ST3 (done) landed signal observation:
`signal_intake` (94, ungated, audited; ops `Enable`/`Disable`/`Take`) opts
the caller's own `Interrupt`/`Terminate` out of default-terminate into one
pending observable event held in `kernel/core::procsignal`, waitable through
the new `WaitSourceKind::Signal` member (id 0, opt-in-checked at add,
targeted `SIGNAL_INTAKE_WAITQ` wake) and drained by `Take`; `Kill` stays
unmaskable, a second pending termination request escalates to the default
terminate (`^C ^C` kills), `Disable` refuses `WouldBlock` while an
observation is undrained, the opt-in is never inherited and is cleared by
the shared task reclaim, and the console-`^C`-to-intake path is proved end
to end by the extended `signal_qemu_aarch64` vertical. ST4 (done) is
`sysmon` (`userland/apps/sysmon`), the fullscreen curses kernel-memory
monitor: a full self-contained store bundle (AppInfo requesting the
`top` surface plus `CAP_MEM_PIN`; thirteen-locale `Help/`) whose model
samples every panel with independent per-query degradation (a refusal is
the panel's stated reason, a hiccuping service never kills the observer),
pins itself at startup with a graceful title-line refusal, and draws six
summary lines (memory, pressure band + history strip, CPU, census) above
a `p`-cycled scrollable detail panel (reclaim ledger, `ramzip` counters,
per-CPU load, interrupt lines, top consumers) on an event-driven bounded-wait loop with
`+`/`-` interval keys; the four kernel-statistics fetches were hoisted
into the shared `tairix_procinfo::kstats` (resolver retargeted), the
GNU `-d` delay grammar into `tairix_curses::delay`, and the viewer figure
formatters into `tairix_procinfo::human` so `top`/`sysmon` share one
definition; proved by exhaustive host model/render/loop tests and the
`sysmon_qemu_aarch64` full-boot vertical (login → `sysmon` → pressure +
reclaim figures on the transcript → `q` → intact prompt). ST5 (done) is
`stress` (`userland/apps/stress`), the stress-ng-style load generator: a
pinned, signal-observing controller dispatching swappable cpu/vm/io/hdd/cache
workers re-entered through the kernel's `@self` spawn token (widened in place
to serve any spawn of the caller's own attested binary), byte targets sized
from discovered RAM / mount free space with `--overcommit` rescale, the
closed GNU-style option set (`--timeout`, `--quiet`, `--background` via a
detached `@self` re-spawn, `--monitor` running the installed `sysmon`),
typed worker refusals counted as expected outcomes (`REFUSED_EXIT` 3, GNU
exit conventions 0/1/130/143), total scratch-tree hygiene on every exit
path, and an fd-3 `summary` record; the stage also landed
`MetaPolicy::stamp_creation` (the secured VFS stamps a created node with
its creator's uid/gid — ARXFS's raw create stamped the system user, locking
creators out of their own files) and `ProcessWait::parent_exited` (the
shared task reclaim severs a dead parent's child rows: zombies dropped,
running children orphaned without breaking `is_live`, an orphan's exit
never strands a zombie); proved by exhaustive host tests (parser, worker
codec, sizing, load units over a scratch seam, every controller teardown
path) and the `stress_qemu_aarch64` full-boot vertical (login → a
`--cpu/--vm/--io --timeout 2s` run → dispatch + successful-completion lines
→ post-load `sysinfo pressure`/`reclaim` renders → intact prompt); the
four-CPU blocking-service path relies on the aarch64 context frame preserving
each continuation's `DAIF`, covered directly by the repeated opposite-mask
`kthread_switch_qemu_aarch64` vertical; the
`RAMZIP_STATS` movement assertion stays behind the §0
restartable-user-fault prerequisite. ST6 is the combined QEMU vertical,
benchmarks, and docs sweep. See `plans/STRESSTEST.md` for the binding design
and staging.

---

## DISPLAY — seat ownership: the display/console locking model (`plans/DISPLAY.md`)

**Status: complete (D1–D6 done).** Closes the console/graphics *ownership
and locking* gap against Linux (DRM master + `logind` seats + tty controlling
terminal), and improves on it under the charter's fail-closed, no-ambient
model. The plan makes the **seat** a first-class kernel object with a tracked,
exclusive, revocable owner, derives the framebuffer
present right from the live lease (coupling scanout to ownership), adds
per-console controlling-terminal/foreground arbitration without signal races,
and models multi-head as N independent seats. Exactly one new capability,
`CAP_SEAT_ADMIN` (the `chvt`/`logind`-equivalent switch/revoke authority),
introduced alongside its `seatmgr` holder and enforcement point (§5.2). D1 is
done: the dependency-free `no_std` `lib/seat` crate is the one owner/lease
state machine (owner-checked acquire/release, observable revocation with
generation-counted leases, and the text-vs-desktop input-routing decision),
host-tested and documented (`docs/src/desktop/seat.md`). D2 is done: the
kernel seat registry (`kernel/core/src/seat.rs`, replacing the owner-less
`InputFocus` arbiter) hosts `tairix_seat::SeatState` under its own lock;
`display_acquire`/`display_release` (numbers 23/24, evolved in place) bind
and check the kernel-attested owner with the typed refusals
`Errno::SeatBusy`/`SeatNotOwner`/`SeatRevoked` (new `abi-v1` errnos 24–26,
generated into the C headers), the desktop `keyboard_read` drain is
owner-gated through `SeatState::access`, and the `CAP_DISPLAY` /
`CAP_INPUT_READ` rustdoc states the enforced behaviour. D3 is done: the
single new capability `CAP_SEAT_ADMIN` (id 33) landed with its two
audited enforcement points — `seat_switch` (70, foreground retarget with
fail-closed seat/console validation) and `seat_revoke` (71, forced
eviction whose record carries the evicted task id; the old owner's next
owner-gated call sees `SeatRevoked`) — and its sole holder, the
`userland/system/seatmgr` service (reserved `SEATMGR_ENDPOINT` broker
requiring each requester's attested `CAP_SEAT_ADMIN`, launched by PID 1,
headless-safe). Seats are observable through the System Information API
(`IntrospectDomain::Seats`, the audited `CAP_SYSINFO_HW` `SEAT_LIST`
query, and `sysinfo seats`). D4 is done: the present right is derived
from the live lease — `display_acquire` returns the minted lease
generation, the `lib/abi` handle (`tairix_abi::seat::SeatLease`) is
threaded to a display driver as its host's `DriverHost::seat_gate()`
(kernel side: `SeatRegistry::present_gate` over the one
`SeatState::verify` definition), and every display driver's
present/flip (framebuffer, vesa, rpi_hvs — software and hardware paths
alike) checks it first, refusing a revoked client with the distinct
`DriverError::SeatRevoked` (`abi-v1` driver error 14) while its
framebuffer mapping persists; a seatless host presents ungated
(headless stays first-class). Proven by driver unit tests and the
aarch64 framebuffer QEMU vertical's seat phase (revoked present
refused, surface intact, new foreground renders). D5 is done: each text
console carries a kernel-tracked controlling (foreground) owner — while
one is recorded, only it may `stream_read` / `stream_input_mode` that
console (any other task sees the typed `Errno::NotForeground`, new
`abi-v1` errno 27, before any input is consumed); `console_foreground`
(72, unchanged number) grants/releases the ownership with layered,
capability-minimal authority (live-child authorisation plus an
owner/granter-checked slot transition on the console device, so a
bystander can neither take nor clear the drain right), and a vanished
owner never wedges the console (the `exit` path releases it; the read
gate clears an owner `ProcessWait::is_live` proves dead — heal, never
widen). D6 is done: the kernel registry hosts every seat independently —
the boot seat (`SEAT_PRIMARY`, id 0) always exists, and a display-class
node published into the live hardware tree mints a seat
(`SeatRegistry::attach_display`, `SEAT_CREATED` 4053) that its removal
destroys (`detach_display` over the removed-ids the `HwTreeSource::remove`
seam now reports, `SEAT_DESTROYED` 4054) — hotplug with no reboot; the
seat-addressed syscalls (`display_acquire`/`display_release`,
`key_inject`/`keyboard_read`) name their seat and fail closed `NotFound`
for a dead or unknown one (the present gate re-resolves the seat per
call), seat ids are monotonic and never reused, and
`SEAT_LIST`/`IntrospectDomain::Seats` pages every seat by whole record.
Proven by the aarch64 framebuffer QEMU vertical's multi-seat phase and
kernel host tests through the real `hw_emit_node`/`hw_remove_node`
handlers. Input-device→seat assignment beyond the boot seat's directly
attached keyboard is seat-manager topology policy, staged with the
desktop session work (CU6).

---

## NEW-DESKTOP-LOGIN — the graphical login screen and session switching (`plans/NEW-DESKTOP-LOGIN.md`)

**Status: in progress — G1–G7 done and G7.1's first vertical (a real boot
reaches the graphical login screen unasked); G7.1 verticals 2–4 remain.**
How a machine reaches a *logged-in* state: the boot-time text-vs-graphical
decision, the first-class graphical login screen (the *greeter*), the
session authority that owns authentication and session lifetime, and
macOS-style fast user switching between concurrently live desktop sessions
on one seat. The plan is binding and carries the design; only status lives
here.

- **G1 — the boot session decision.** `lib/supervisor`'s `continue`
  takes an optional `text` | `gui` operand (`console` / `graphical` /
  `desktop` accepted, case-insensitive; anything else refused with the
  usage line and the REPL held open). The choice rides
  `SupervisorExit::ContinueBoot(BootSession)` into the root-unlock path,
  which installs it once into a set-once kernel cell; the ungated,
  unaudited `boot_session_get` syscall (no. 107, the `boot_facts_get`
  boot-static-public-value shape) reports it, with
  `tairix_rt::boot_session()` failing closed to `BootSession::Unset`.
  `login` combines it with the stored `os.loginType` through the one
  `effective_session_kind` precedence — the operator's one-boot choice
  wins, else the store, else the compiled default — and still degrades to
  text when no graphical session is available.
  **The compiled default is `graphical`:** hardware that can run a
  graphical login gets one unconfigured, and `SystemConfig::default()
  .login_type` is the single definition every unlearnable-store path
  (absent, unreadable, non-UTF-8, unparseable) resolves through, so the
  default cannot be flipped in one place and ignored in another. The
  degradation to text is what makes that safe and its tests are
  load-bearing; a QEMU vertical that wants the text prompt on drawable
  hardware plants an `os.loginType text` document rendered by the
  configuration engine itself rather than relying on a default.
- **G2/G6 — one authentication surface.** **`lib/greeter`** (new `lib/*`
  crate, registered in `AGENTS.md` §3) owns the full-screen "prove who you
  are, at the screen" surface: the `AuthSurface` state machine, the one
  `layout` geometry both paint and hit-test read, the wording, the bounded
  secret and its wipe on every terminal transition, and the render over a
  caller-supplied `Backdrop`. The screen is one centred column — clock,
  date and host name above a monogram disc, the account's name, a pill
  secret field, and one notice line — every length authored logical and
  converted through `Scale` exactly once, with the chrome's presence a
  function of the screen and density alone so it cannot appear or vanish
  between choosing an account and typing. Authentication itself
  stays with the embedder through the `Verifier`/`Verdict` seam, so the
  engine takes no ABI or IPC dependency. The desktop session's screen lock
  composes it and keeps only its embedder duties (the compositor window,
  `keep_topmost`, `LockedDrain`, the elevation-broker `Verifier`).
- **G4 — `session-v1` and the session authority.**
  `tairix_abi::session_ipc` is the reserved rendezvous `login` binds and
  the three requests it answers: paged `Accounts` (display name, login
  name, live flag — nothing a tile does not draw), `Authenticate`
  (a verdict; it starts nothing), and `Background` (G5). Placement (the
  attested console) is checked before any state, then a per-request
  identity rule — the greeter's uid for the first two, the uid owning the
  foreground session for the third. Every refusal is byte-identical and
  carries only the remaining cooldown, an unauthorised or undecodable
  request gets a well-formed empty page rather than an errno, and the
  request buffer holding the secret is wiped on every path.
  `login` gained the `AttemptBudget` (per login name, monotonic, three
  free attempts then 5 s doubling to a 5-minute cap, over a bounded
  16-entry table that evicts only expired entries), the graphical round
  (spawn the greeter as its own account on login's own console, serve
  `session-v1` from the same wait-set, three consecutive greeter failures
  degrade the round to text), and the availability probe extended to the
  greeter bundle.
- **G3 — `greeter.app`.** `userland/session/greeter`, planted at
  `/System/Services/greeter.app` by bundle discovery, running as the new
  dedicated `greeter` service account (uid 16, `GREETER_CEILING`). It
  draws and types; it never decides: seven capabilities, and deliberately
  no `CAP_USERS_READ`, no `CAP_PROC_SPAWN`/`CAP_SPAWN_AS_USER`, and no
  privileged bind — compromising it yields a screen, not an account. It
  owns the seat, composes `lib/greeter`, decodes its wallpaper in a
  capability-empty sandbox worker, draws the pointer through the hoisted
  `tairix_cursor::PlacedCursor`, presents only the damage rectangle, and
  parks with no timer at all when nothing is counting down.
  **Pointer motion costs no render and no round trip.** The service keeps
  the *clean* rendered surface and re-renders only when the surface's own
  state changed — a closed set (its state, screen, scale, backdrop), which
  is why the cache cannot go stale — so `Repaint` is three cases (nothing /
  cursor-only / painted) and the cursor is instead sampled over the cached
  pixels at scan-out. A drain applies every queued report and presents
  **once**, merging damage through the shared `sub_screen_damage`
  classification. A bare move is then a hit test, two rectangle unions and
  a cursor-sized copy: no allocation, no glyph, no wallpaper blit.
- **`CAP_SANDBOX_SPAWN` (new, id 43).** Isolating untrusted input must not
  cost the authority to start a general process, so the narrow capability
  admits *only* a canonical parser sandbox — a child the kernel brands
  capability-empty, with no credential switch or console inherit. `spawn`'s
  dispatcher gate therefore moves into the handler, which alone decodes the
  attach block: a coarse "holds one of the two" refusal before any staging,
  then sandbox ⇒ either capability, anything else ⇒ `CAP_PROC_SPAWN`, with
  `SpawnMode::admits` the one definition and both refusals audited before a
  page table exists. Granted to the greeter alone; every other sandbox user
  already holds the broad capability that subsumes it.
- **G5 — fast user switching.** `login` keeps the live-session table (one
  `Foreground` at most, the wake mailbox *derived* from the session's task
  id, never stored); a desktop steps aside with `Background` and is
  resumed through `SessionWake::Foreground`, re-acquiring the seat and
  re-moding the compositor (`Compositor::set_mode`, new) for a display
  mode that may have changed. If the authority itself exits it drains the
  table newest-first with `SessionWake::End`, so a background session is
  never orphaned unreachable. The desktop offers it as `Switch User…`,
  absent rather than broken when the mailbox could not be bound.
- **Shared, not duplicated:** the scan-out encode (frame length, channel
  order, damage-vs-whole-frame) moved out of the window manager into
  `lib/display::scanout`, cursor *placement* (origin = pointer − hotspot,
  sampling under a screen row) out of it into
  `tairix_cursor::PlacedCursor`, and the separable box blur *and the frost
  built on it* out of it into `tairix_raster::{box_blur, frost_region,
  BlurScratch}` — so the compositor's window backdrop and the login screen's
  selected tile are literally the same call, and `userland/session/*` needs no
  forbidden edge into
  the window manager to draw a pointer or frost a backdrop. `lib/font` gained
  the matching text fitters every too-narrow label now shares:
  `elide_to_width` and the lazy, non-allocating `wrap_to_width`, over one
  `ELLIPSIS` mark that is both measured and drawn.
- **The account tile is the shared `IconTile`, improved for everyone.** A
  selected tile **frosts its own backdrop** — the pixels it covers, a window's
  surface or the desktop wallpaper, blurred by the scaled
  `selection_backdrop_blur` (6 logical px) through the same `frost_region` the
  compositor frosts a window
  with — and the theme's `selection_fill`, its own accent at three tenths
  opacity, is laid over that with a **crisp** edge, rounded like every other
  plate. The fill is that light because the frost is what marks the item; the
  accent only tints it. Frost
  and fill are confined to that one rounded shape, so nothing escapes the tile's
  bounds and no square edge shows around the rounded fill. Softening the *fill*
  instead leaves a smear with no shape of its own: the blur belongs behind the
  mark, not on it. The radius is short on purpose, and bracketed by rendering
  tests from both sides rather than pinned to a number: a box blur of radius
  `r` averages `2r + 1` samples, so a radius approaching the item's own size
  averages its whole backdrop to one colour and the mark becomes a smudge with
  an accent cast. It must take the backdrop's fine grain and leave its larger
  shapes legible.
  The mark **cross-fades** as the selection moves, over the theme's new
  `MotionInteraction::SelectionChange` (100 ms): the tile being left decays while
  the tile arrived at grows, driven by `IconTile::with_selection_fade` from the
  owner, off the login screen's existing park deadline, so an idle screen still
  arms no timer and a reduced-motion theme settles the change at once. The
  strength scales the frost and the fill together, so a backdrop never snaps
  into focus ahead of the colour leaving it. The name
  keeps the ordinary foreground, which is the ink that reads over a part-opaque
  tint; the on-accent inversion and the crisp opaque panel belong to a
  high-contrast theme, where a selected tile frosts nothing and does not fade.
  A **selected** tile draws neither the focus ring nor the pointer wash,
  whatever strength its mark is at — both follow the selection, never the
  strength, because an outline that showed for as long as a mark took to arrive
  read as a border flickering under the pointer. The label **wraps**
  over as many whole lines as the band holds, centred, only the last elided, and
  `IconTile::label_lines` exposes that budget so an owner sizes a tile from
  the render's own geometry — which is how the greeter's tile is 132 × 154
  (three whole lines at 100% *and* 200%), not a guess.
- **The login-to-desktop transition is animated end to end**, off one shared
  primitive: `tairix_theme::Timeline` — start from the theme's duration, read
  `progress` (linear) or `eased` (smoothstep, for anything that travels), read
  `next_frame_in` to know when to wake. It reads no clock; the embedder passes
  the monotonic instant it already holds. A zero duration starts *settled*, so
  reduced motion needs no second code path and an idle surface arms no timer at
  all. Four new/rehomed animations, every duration theme data
  (`MotionInteraction`): the chooser's `SelectionChange` cross-fade (100 ms,
  re-expressed on the timeline, its hand-rolled clock arithmetic deleted); a
  `StageTransition` (240 ms) that carries the picked account's monogram from
  its tile to the prompt's disc while the other tiles fade out and the prompt
  fades in, in **both** directions, interpolating the *layout* rather than
  cross-fading two screen-sized renders; an `AttemptRejected` (420 ms) decaying
  shake on a refused secret, ending at exactly zero offset, *additional* to the
  notice and cooldown; and a `SessionFade` (1000 ms) that fades the login
  screen to black **before** it exits and reveals the desktop from black
  afterwards. The reveal is one `Compositor` property applied where composed
  pixels become the scan-out frame, so no pixel can reach the display
  undimmed — and `encode_layers` declines while it runs, because a hardware
  layer the display scans out directly would never pass through it. The greeter
  fade is total: a lost display or a failed present still exits `0`, because a
  cosmetic fade may never strand a successful login.
- **`CAP_LOG_EMIT` is now part of `SESSION_BASELINE`.** The desktop announces
  itself visible once the reveal completes — a one-shot diagnostic record the
  QEMU verticals key their screendump on, because the first presented frame is
  now deliberately black and can no longer witness "the composited desktop
  reached scan-out". No interactive ceiling carried `CAP_LOG_EMIT`, so the
  kernel discarded every record a session emitted. The cost is stated rather
  than glossed: any program a logged-in user runs may now write to the
  machine-wide **diagnostic** log, so log noise and provenance confusion are
  possible. The hash-chained audit log is a separate capability and stays
  kernel-only, and the kernel — never the caller — attributes each record, so
  nothing here lets a user program forge, alter, or truncate an audit entry.
- **Fixed in passing:** the desktop's screen lock was handing the shared
  authentication surface a frozen clock (`now_ns: 0`), silently settling every
  animation it has; it now runs on the session's real monotonic clock, so the
  lock screen animates like the login screen it shares an engine with. And the
  session had been built to log since long before this work — it passes a log
  sink into its cache ledgers — while structurally unable to: every one of
  those records was discarded, silently, because the write path is best-effort.
  The grant above is what makes them arrive.
- **Done — G7.1 vertical 1, a real boot reaches the login screen unasked**
  (`tests/integration/greeter_default_qemu_aarch64`). It boots the aarch64
  `virt` board with a display and the signed input/display driver bundles on
  `FsDisk::GreeterRootDisk` — the autoload driver store with the *standard*
  application store, so no `os.loginType` is planted and the machine is in
  the state a fresh installation boots in — types the unlock passphrase and
  nothing else, and passes only on two kernel-attested witnesses: an
  `APP_LOADED` naming the greeter's bundle, then a reply the display service
  serves on `DISPLAY_ENDPOINT` after it.
- **Fixed by that vertical: a never-configured machine was pinned to the
  text prompt.** Login distinguished "the settings volume is not mounted"
  from "this machine holds no configuration" by probing the store's
  *directory* — a directory `configure` only creates on its first write. A
  fresh installation has none, so every capable machine read as an offline
  volume and withheld the compiled `graphical` default, making the
  `Reachable(None) ⇒ graphical` rule dead in production. The two are now
  told apart by the refusal one read of the document returns
  (`ConfigStore::from_read`): a mount with no registered backing fails
  closed `NotImplemented` and never falls back to another volume, so that
  refusal alone means "ask again later"; every other refusal came from a
  live volume that teaches nothing. One read replaces two syscalls.
- **Remaining: G7.1 verticals 2–4** — authenticate onto the desktop, log
  out, switch accounts. They need a scripted authentication *at the login
  screen* (a pointer script and credentials reaching the greeter's own
  field, not the console type-ahead the unlock prompt drains) plus
  screendump assertions over the desktop that follows; the display-and-input
  harness itself now exists. **The gap is not theoretical:** the first real
  boot to this screen showed no wallpaper, no text, and no pointer while
  every crate was host-green, because a capability refusal, an unlinked
  glyph transport, and an undrawn cursor are all invisible to a host render
  test. Vertical 2 must therefore assert on content — text present, the
  wallpaper drawn, a pointer visible — not merely that a frame arrived.

---

## SMARTRAM — reclaimable memory services (`plans/SMARTRAM.md`)

Opportunistic, bounded, owner-accounted caches over spare RAM;
`plans/SWAPSWAPSWAP.md` owns the encrypted compressed anonymous tier.

**Done — SMART1 classification/accounting model and the clean,
rebuildable filesystem cache (`plans/SMARTRAM.md` SMART1 + §6.1):**
- `tairix_reclaim::model` (`lib/reclaim`, hoisted out of `kernel/mem` so the
  desktop session obeys the same model): the reclaimable-memory model — the complete
  nine-class `ReclaimClass` taxonomy with deterministic
  `reclaim_priority` in the §7 pressure order (disposable/speculative
  classes first, `CleanFileData` before `TransformCache`, `FsMetadata`
  and `ReliabilityAssist` preserved longest); the `ReclaimOwner` model
  for the owners the kernel already has (kernel subsystem, filesystem
  volume by stable per-boot mount handle, task);
  `RebuildCost`/`Sensitivity`/`InvalidationSource`/`ReclaimRule`
  modelling; the fixed `MAX_ENTRY_METADATA` per-entry bookkeeping
  validation bound; the pure, fail-closed `CacheCandidate::classify`
  admission gate with typed `AdmissionRefusal` reasons (unknown class,
  unknown owner, sensitive material — credential/key/capability and
  undeclared sensitivity alike — unbounded metadata, non-reclaimable,
  missing invalidation); `CacheBudget::from_backing` (per-volume hard
  limit = 1/16 of the kernel heap arena; shrink watermark = 3/4 of hard
  — hysteresis); and the checked, fail-closed per-class
  `CacheAccounting` ledger with hit/miss/insertion/invalidation/
  eviction/refusal counters.
- `kernel/core::fs::CachedFs`: per-volume write-through cache below the
  secured VFS (permission checks never bypassed), wrapping each driver at
  registration (`system_mount::cached`), its two candidate declarations
  classified through the admission gate at construction and charged to
  the volume's `ReclaimOwner` (a refusal starts the cache poisoned —
  fail closed, the driver keeps serving). Caches page-chunk file data,
  stat, security, lookup, and dirent records; LRU eviction (data before
  metadata); large reads (> 4 chunks) bypass; payload allocations are
  fallible; every cached buffer is zeroed on invalidation/eviction/purge/
  teardown (the volumes are encrypted at rest); an unidentifiable
  mutation target purges the cache and a ledger imbalance poisons it
  (fail closed, driver keeps serving).
- **Single-writer coherence fix (pre-existing corruption hazard):** the
  root volume was opened read-write **twice** (the `fs_*` driver plus a
  second `ARXFS` window for the `CAP_USER_ADMIN` engine), which could
  double-allocate COW clusters and made any cache unsound.
  `FilesystemSecurity::set_security` moved into the abi trait (abi-v1
  unfrozen, in-place evolution), the `AdminFs` trait was deleted,
  `LateFilesystem::register` returns the leaked per-mount `SleepLock`,
  and `RootAdminBacking` now shares the one registered (cache-wrapped)
  driver — one volume, one writer, every mutation visible to the cache.

**Done — SMART2 VM pressure bands and reclaim ordering
(`plans/SMARTRAM.md` SMART2 + §7):**
- `tairix_reclaim::pressure` (with the `ramzip`-handoff and escalation half
  left in `kernel/mem::pressure`): the complete pressure-state model (none
  existed) — the five-band `MemoryPressure` gauge (normal/mild/
  moderate/severe/critical, the one vocabulary shared with
  `plans/SWAPSWAPSWAP.md`) over a `FreeMemorySource` (production: the
  physical `FrameAllocator`), sampled on the consumers' own operations
  (no background workers, no tick), with per-band enter/exit
  watermarks derived from the backing size (hysteresis; benchmark-
  tunable fractions, never ABI), a reserve floor (1/64) below which
  every reading is critical, fail-closed critical on a zero/unknown
  backing, and `growth_permitted` (growth only at normal pressure,
  never into the reserve). The pure policy layer: `shrink_target`
  (per-band per-class ceilings in the §7 order — disposable/
  speculative drop at mild, clean file drains with transform at
  moderate, metadata/recovery-assist preserved to the low watermark,
  severe/critical force zero, monotone with depth), `ramzip_handoff`
  (compression only from moderate, and at moderate only once
  clean+transform are drained; critical belongs to escalation), and
  the deterministic `escalation` order (reclaim caches → hand off to
  `ramzip` → VM policy) — the seams SWAP3 binds to.
- `kernel/core::fs::CachedFs` consumes the gauge: the boot path builds
  one gauge over the leaked frame allocator (`root_unlock` →
  `UnlockEnv` → `install_system_mount`/`register_writable_state` →
  `system_mount::cached`), every cache-touching operation applies the
  band's forced-shrink targets before serving (data before metadata,
  evicted buffers zeroed), and admission is refused outside normal
  pressure — the driver always keeps serving.

**Done — SMART3 filesystem metadata and transformation caches
(`plans/SMARTRAM.md` SMART3 + §6.2; `docs/src/architecture/memory.md`
§7i):**
- The metadata cache is the SMART1 `CachedFs` (already live on every
  registered volume); extended-attribute/type-detection caching is
  deliberately not built — the mounted kernel filesystem surface
  (`KernelFs`) has no attribute or type-detection consumer today, and
  the stage is scoped to current consumers.
- The ARXFS transform cache: the driver exposes an injected
  `ClusterCache` seam (`drivers/filesystem/arxfs`'s `xform` module —
  keyed by the stored run's first physical block, consulted only in the
  serving read path, never by scrub/check/rescue; invalidation funnels
  through the single block-free choke point, rollback purges, a
  zero-progress entry fails the read closed) and the kernel implements
  it (`tairix_kernel::transform_cache::TransformClusterCache`):
  classified through the SMART1 gate as `TransformCache` owned by the
  volume's mount handle, LRU-bounded with hysteresis under a
  1/16-of-heap `CacheBudget`, pressure-enforced per operation
  (preserved at mild, drained from moderate before any `ramzip`
  handoff, growth only at normal outside the reserve), volatilely
  wiping every released buffer. Installed on both boot volumes
  (`system_mount` for `/System`, the aarch64 unlock path for the
  writable root). The driver also wipes its transient decrypted
  frame/plaintext scratch on every cluster read, clone, and decompose
  path.

**Done — SMART4 semantic application-launch cache
(`plans/SMARTRAM.md` SMART4 + §6.3; `docs/src/architecture/memory.md`
§7j):**
- `kernel/core::launch_cache::LaunchCache`, held by the `AppStore`
  behind the `/System`-mount readiness latch: retains the shared
  `lib/appload` gate's accepted `LoadedApp` (parsed signed manifest,
  content-hash + interface-hash verdicts, dynamic-loader policy
  decisions, validated `rxe` image) for immutable read-only
  system-store bundles, once per boot. Classified through the SMART1
  gate as `SemanticAppCache` owned by `KernelSubsystem("app_store")`,
  LRU-bounded with hysteresis under the kernel-heap-derived
  `CacheBudget`, pressure-enforced per operation (low watermark at
  mild, drained from moderate before `ramzip` handoff, growth only at
  normal outside the reserve), fail-closed (uninstalled/refused/
  poisoned cache ⇒ every launch runs the full gate).
  `install_system_mount` installs budget + gauge before resolving the
  latch; the old ad-hoc RAM-divisor bundle cache was deleted. A hit is
  caller-independent (manifest-request ceiling; per-caller capability
  intersection at admit; the caller's VFS read of the entry point is
  re-authorised per launch).
- Scope decisions recorded in the plan: command-resolution *output*
  caching (pure spelling in `lib/cmdres`, cheaper to recompute) and a
  separate RXE relocation-preparation cache (no relocation stage
  exists; the cached image is the validation state) are deliberately
  not built.

**Done — SMART9 observability through existing diagnostics
(`plans/SMARTRAM.md` SMART9 + §11; `docs/src/architecture/memory.md`
§7k):**
- `CacheAccounting` splits every class ledger into payload and
  per-entry bookkeeping metadata (`class_payload_bytes` /
  `class_metadata_bytes`) and adds `pressure_shrinks` / `teardowns` /
  `failures` beside the existing event counters; `MemoryPressure`
  counts entries into each band (`band_entries`, swap-exact per stored
  change).
- Those counters reach a reader through the audited
  `CAP_SYSINFO_KERNEL` queries, and the export covers **both** halves of
  the model. Each cache describes itself with one `CacheLedger` and one
  conversion to the `CacheLedgerRecord` wire row; the kernel exports its
  own rows per cache, a process reports the caches only it can see
  through the ungated self-scoped `CACHE_REPORT`, and `sysinfod` folds
  the two sets into the per-class `RECLAIM_STATS` totals it also serves
  per cache as `CACHE_LEDGERS`. Reported rows stay in the service, never
  the kernel, so a self-reported figure cannot reach a kernel reclaim
  decision; each is stamped with the caller's attested identity, keyed by
  its unforgeable process instance, and rendered marked as claimed rather
  than measured.
- `tairix_reclaim::audit` owns the subsystem's stable audit events
  in kernel/mem's reserved `2_000..3_000` `EventId` range:
  `RECLAIM_CACHE_REFUSED` (2000) and `RECLAIM_CACHE_POISONED` (2001),
  with a closed `cache`/`owner`/`owner_id`/`cause` field shape (fixed
  labels and numeric handles — never a filename, plaintext, key, or
  token). All three caches (`CachedFs`, `TransformClusterCache`,
  `LaunchCache`) take the boot audit sink at construction (threaded
  through `system_mount`, the root-unlock path, and
  `AppStore::install_reclaim`) and report a poisoning exactly once;
  normal operation emits nothing.

**Done — SMART10 cross-cache integration, thrash, and benchmark
evidence (`plans/SMARTRAM.md` SMART10; `docs/src/architecture/memory.md`
§7l):**
- `kernel/core/src/reclaim_integration_tests.rs`: the production
  `CachedFs` and `LaunchCache` on **one** shared gauge through the full
  band order; the `ramzip` handoff computed over the caches' combined
  clean+transform residue (held while any remains, open once their own
  operations drain it, never at critical — escalation yields the VM
  policy); the shared reserve floor; no stale serving for a file
  mutated while the caches were drained; the thrash scenario (band
  flapping inside the hysteresis window causes zero rebuild churn,
  detected via the SMART9 counters); and the work-avoided benchmark
  evidence (warm passes deterministically perform zero driver reads /
  load-gate runs; wall-clock numbers printed as estimates).
- The layered stack in `kernel/tairix-kernel`'s transform-cache suite:
  `CachedFs` over a real ARXFS volume consulting the installed
  `TransformClusterCache` on one gauge — a filesystem-cache hit never
  reaches the transform layer; moderate pressure drains both layers
  while correct bytes keep being served.
- Shared test fixtures live once (`kernel/core/src/test_pressure.rs`;
  the bundle-verification helpers in `kernel/core/src/test_bundle.rs`),
  replacing the per-suite copies. A dedicated QEMU pressure vertical is
  deliberately not built (the band arithmetic is pure and host-proven;
  the sampled frame allocator is already soaked by
  `memsoak_qemu_aarch64`).

**Done — SMART11 whole-disk block-level LRU cache
(`plans/SMARTRAM.md` SMART11; `docs/src/architecture/memory.md`
§7m):**
- `kernel/tairix-kernel::block_cache::BlockCache` wraps the one
  brought-up boot disk **below** the block-sharing layer
  (`shared_block::SharedBlock`, on the device side of its sleep lock),
  so every window onto the disk — the `/System` driver-store window,
  the encrypted-root unlock window, and the writable-root window —
  reads through one coherent per-block LRU cache; installed by the
  shared `finish_unlock` boot tail for both the virtio-blk and EMMC2
  bring-ups, on the same gauge and audit sink as the volume caches.
- Classified through the SMART1 gate as `CleanFileData` owned by the
  `boot_block_device` kernel subsystem (a refusal or an unboundable
  block size poisons it to pure passthrough — fail closed),
  pressure-enforced per operation (low watermark at mild, drained
  from moderate before `ramzip` handoff, growth only at normal
  outside the reserve), LRU-bounded with hysteresis under the
  kernel-heap-derived `CacheBudget`. Write-through coherence: a
  successful write refreshes cached copies in place, a failed write
  or discard invalidates its range, large reads bypass, and
  `BufferClass::Sensitive` I/O bypasses *and* evicts its range so no
  key-slot block is ever retained; every released buffer is wiped.

**Done — SMART5 desktop and UI cache integration
(`plans/SMARTRAM.md` SMART5 + §6.4; `docs/src/architecture/memory.md`):**
- The reclaimable-memory model is now **shared**, not kernel-private:
  the classification taxonomy, budgets, checked accounting, audit
  events, band vocabulary, hysteresis thresholds, and `shrink_target`
  ordering moved from `kernel/mem` into the new `lib/reclaim`
  (`tairix-reclaim`), which the kernel and userland both import. Only
  what genuinely needs the anonymous-memory tier stayed behind in
  `kernel/mem::pressure`: the `ramzip` handoff, the escalation ladder,
  and the frame allocator's `FreeMemorySource` binding.
- One `PressureGauge` interface, two vantage points: `MemoryPressure`
  measures free memory (kernel), `ReportedPressure` holds the band it
  was told (userland) and answers `critical` until told — an unwired
  process admits nothing rather than assuming the machine is
  comfortable.
- One cache implementation, `tairix_reclaim::ReclaimCache<K, V, E>`:
  bounded by a derived `CacheBudget`, invalidated wholesale by a
  generation token, shrunk to the band's `shrink_target`, LRU by an
  O(log n) recency index, wiping every non-public entry on release,
  charging a checked payload/metadata ledger, and self-poisoning
  (draining and serving uncached) if its books stop balancing. A
  refusal still returns a usable value (`Served::Uncached`), so
  caching is never required for correctness and no path renders twice.
- Event-driven delivery, never polling: `WaitSourceKind::MemoryPressure`
  (9, `id` 0, no capability) is edge-triggered on the published band;
  the gauge's `BandObserver` flags `kernel/core::waitq::PRESSURE_WAITQ`
  lock-free (it fires inside allocation paths) and the real unpark runs
  at the next dispatcher-context drain. The ungated, unaudited
  `SysinfoQueryId::MEMORY_PRESSURE_BAND` (28) drains the edge without
  taking a reading; the gated, audited `MEMORY_PRESSURE` view is
  unchanged.
- The desktop's four rasterised-asset caches — the cursor cache, the
  notification-glyph cache, the session's pinned-application artwork
  cache, and the compositor's window-furniture cache — are
  `ReclaimCache`es sharing one classification
  (`tairix_reclaim::desktop`): `DisposableUi`, owned by the seat,
  `UserData` (so entries are wiped), invalidated by the scale/theme
  generation, dropped on reclaim, with the budget derived from the
  discovered framebuffer byte size, so a 4K output is allowed
  proportionately more than a small panel and no ceiling is guessed. The
  first three take a fraction of that output (`disposable_ui_cache`);
  furniture takes a whole screenful (`screenful_ui_cache`, which the
  compositor's frosted backdrops now share), because no
  more chrome than fills the screen can be visible at once and the
  surplus is exactly what reclaim should take first. The session parks on
  the band member, trims all four on a change, and tears them down on
  logout or seat loss.
- Rasterised glyphs are the same model on both sides of `FONT_ENDPOINT`:
  one shared declaration (`tairix_font::glyph_cache`) gives the client
  and `fontd` a byte-bounded `ReclaimCache` whose budget derives from the
  machine's total RAM through the ungated `SysinfoQueryId::MEMORY_TOTAL`
  (29), so a caller cannot grow the service by walking the permitted
  glyph-size range and an unknown total caches nothing rather than
  guessing a ceiling. `fontd` parks on the band member alongside its
  endpoint.
- A window's *content* is a pressure-driven release policy rather than a
  keyed cache, because evicting a visible window's pixels is a visual
  defect and not merely a slowdown. It reads the same gauge and the same
  shrink ordering: hidden and minimised windows are released at mild
  pressure, visible unfocused ones only at critical, the focused window
  and session-painted surfaces never. A release wipes the pixels and
  raises `WindowEvent::RedrawRequested`, which `lib/window` answers for
  the app by re-presenting its last frame.

**Remaining (staged, `plans/SMARTRAM.md` §12):** the non-ARXFS
transform families (verified bundle/manifest state gated on their
consumers) — gated on the subsystems they consume.
The reliability/background/predictive caches (SMART6–8) are
**shelved — not added**; they are built only if a future decision
explicitly un-shelves them.

---

## SWAPSWAPSWAP — the encrypted compressed anonymous-memory tier (`plans/SWAPSWAPSWAP.md`)

**Done — SWAP1–SWAP4 (the `ramzip` tier as the complete arch-neutral
VM mechanism, `kernel/mem::ramzip`;
`docs/src/architecture/memory.md` §7n):**
- Shared sealing primitives hoisted into `kernel/mem::seal`
  (`SealKey`, `EntropySource`, `NonceSequence` — one definition of the
  per-boot-key/zeroize-on-drop/salt-plus-counter-nonce discipline,
  consumed by both the encrypted block-swap layer and `ramzip`; the
  two tiers hold separate keys and share no metadata format). `SwapKey`
  was deleted in favour of `SealKey` (in-place evolution).
- SWAP1: the fail-closed eligibility classifier (`PageKind` /
  `PageCandidate` / `Ineligible`; unknown is ineligible), the derived
  capacity policy (`RamzipCaps`: min = max(1% RAM, 64 MiB) clamped to
  hard, soft 10%, hard 25%, per-band ceilings, half-cap per-task
  share, the decompression floor), and the checked all-or-nothing
  global + per-task ledger with saturating diagnostic counters
  (`RamzipLedger` / `RamzipCounters`).
- SWAP2: compress-before-encrypt sealed-page store (`lib/compress`
  then `lib/crypto` AEAD; identity-binding AAD of space/page/flags;
  incompressible pages refused, never stored raw; plaintext
  temporaries zeroed on every path; metadata validated before any
  cryptography; audit events 2002/2003 on auth/decode failure).
- SWAP3: the tier (`Ramzip`) over the real
  `AddressSpace`/`PhysMap`/`FrameAllocator` surfaces — `compress_out`
  gated by the SMART2 `ramzip_handoff`, thrash state, eligibility,
  band cap, task share, and the decompression floor (compression can
  never cause reserve exhaustion); move-only `fault_in` restoring
  exact bytes and mapping flags, discarding the entry, and failing
  closed (no plaintext) on authentication/decode failure; ledger
  releases use the figures charged at compression time, never a
  length recomputed from the corruptible blob (a defect the fuzz
  harness caught, fixed with its regression test); deterministic
  `escalate_refusal` (reclaim caches → VM policy).
- SWAP4: bounded post-fault clustering (±8 pages, same space, sealed
  within 32 events, budget 8 pages, failures never fail the original
  fault), the budgeted `warm_step` (8 pages per step, candidates only
  near recent demand faults, re-gated per page on the
  `warmup_start`/`warmup_stop` hysteresis watermarks added to
  `PressureThresholds`, instant stop on any pressure transition,
  `NothingToDo` without locality evidence — cold pages stay
  compressed), and the deterministic event-clock thrash detector
  (per-task recent-cycle scoring with halving decay; a thrashing
  task's pages are refused until forgiven).
- Tests: the full plan §18 matrix as host tests (eligibility classes,
  caps/reserves/fair-share, round-trip byte/flag fidelity,
  zero-on-free scrub, tamper/truncation fail-closed with balanced
  books, no-leak compress/fault cycles, cluster/warm gates and
  budgets, thrash detection, escalation determinism, nonce
  uniqueness/exhaustion) plus the seeded `fuzz_ramzip` harness
  (registered in `cargo xtask fuzz`) driving random
  compress→tamper/truncate→fault cycles.

**Live-task enablement (staged in `.junie/swapswap-progress.md`):**
- **Restartable user page faults — present.** `kernel/core::resolve_user_fault`
  makes a not-present user page resident (stack growth, demand-paged
  anonymous `mem_map`, read-only file mappings) and retries the faulting
  instruction.
- **Cold-page identification — arch-neutral core landed (b1).** The
  page-replacement referenced-bit facility is in the Arch HAL:
  `tairix_arch_api::mmu::AddressSpace::test_and_clear_accessed` with its
  honest `AccessTracking` declaration (fail closed when a port exposes no
  referenced bit), the `HostPageTable` software model, and the
  `kernel/mem::coldscan` second-chance (clock) scanner (host-tested).
- **Live wiring — landed (b2/b2c), fault-in, infra, AND compress-out
  trigger.** `ramzip` is now a single **process-global** pool
  (`kernel/mem::ramzip::global`, one `Ramzip` behind a `SpinLock`, installed
  once at boot from the seeded CSPRNG + discovered RAM). `LiveSpace` owns a
  `space_id` + `ColdPageScanner` and the object-safe `ramzip_fault_in` /
  `ramzip_reclaim`; a dead space's entries are purged on `LiveSpace::drop`.
  `resolve_user_fault` restores a compressed page **before** the anonymous
  handler (`Fatal` terminates only the faulting task, fail closed), and — the
  b2c compress-out half — calls `ramzip_direct_reclaim` at the top of the
  fault path: TAIRiX's foreground **direct reclaim**, which at moderate/severe
  pressure compresses a bounded batch (`pressure::ramzip_reclaim_batch`: 32 /
  128 pages) of the faulting task's own cold anonymous pages out into the
  tier and re-freezes its snapshot, gated on the pinned/real-time template,
  the clean+transform residue, and the tier's own caps/reserve — fail closed,
  one bounded pass per fault, never a spin. `RAMZIP_STATS` reports the live
  tier (`memstats::install_global_ramzip_stats`). Host-tested end to end over
  `HostPageTable`.
- **Per-port enablement (b3): done on every MMU-bearing Tier-1 port.** The
  trigger, policy, template, residue gate, and snapshot republish are
  port-agnostic and host-tested, and each MMU port now declares
  `AccessTracking::Supported` and reclaims cold anonymous pages end to end,
  proven by its own QEMU vertical
  (`accessed_bit_qemu_{x86_64,aarch64,riscv64}`).
  - **x86_64**: `test_and_clear_accessed` reads/clears the hardware Accessed
    bit (PTE bit 5, `flags::ACCESSED`) with an `INVLPG` — no software fault
    path.
  - **aarch64**: the Access Flag (AF, descriptor bit 10) is software-managed
    (cortex-a72/Pi lack HAFDBS); `test_and_clear_accessed` clears AF (+ TLBI)
    and the synchronous-exception path (`fault::is_access_flag_fault` →
    `paging::set_accessed_flag_in_active`) sets it back on the Access-Flag
    fault and retries. The vertical runs on cortex-a72, so the software path
    is genuinely exercised.
  - **riscv64**: the Accessed bit (A, PTE bit 6) is software-managed under
    Svade; `test_and_clear_accessed` clears A (+ `sfence.vma`) and the trap
    path (`paging::set_accessed_flag_in_active(stval, AccessKind)`) sets A
    (and D for a store) back **only** on a valid leaf that permits the
    access — a permission fault sharing the same `scause` is never masked —
    and retries. The riscv64 QEMU runner pins `svade=true,svadu=false`, so
    the software path is genuinely exercised (not shadowed by hardware A/D
    update); TAIRiX maps every leaf A/D-set, so ordinary operation never
    faults and the existing riscv64 verticals still pass.
  - **wasm32** keeps the fail-closed `Unsupported` default permanently (the
    sandbox exposes no referenced bit).
- **Warm-restore enablement (b4): done — clustering + warm-up are live.**
  The SWAP4 read-half optimisations are now driven from the fault path, not
  only host-tested at the tier: after `resolve_ramzip_fault` restores a page
  it samples pressure once and, only while memory is comfortably free, runs
  fault clustering around the faulted page and one bounded warm step over
  entries near recent faults through the object-safe
  `LiveUserSpace::ramzip_cluster` / `ramzip_warm` seams, re-freezing the
  snapshot once if any page was brought back. Foreground-only (charged to the
  resuming task, no daemon, no spin), comfort-gated and reserve-safe (never
  under pressure, decompression floor re-checked per page), best-effort (a
  cluster/warm failure never fails the original fault). Host-tested over
  `HostPageTable`: clustering restores exactly the contemporaneous neighbours
  when comfortable and nothing under pressure; warm-up restores near recent
  faults only with locality evidence and comfort and stops instantly under
  pressure; both are fail-closed no-ops on an empty tier.
- **Performance evidence (b5): done — the §19 requirement is met.** Host
  benchmark tests (`kernel/mem::ramzip::tier::tests::bench_evidence_*`) prove
  the work avoided — a compressible cold page shrinks far below its logical
  size (≈ 94 % saved observed) and a move-only fault-in leaves no duplicate
  copy or leaked frame — and print compress-out / fault-in / cluster /
  severe-band / incompressible-refusal latency *estimates* across a Pi-class
  (2 MiB) and a desktop-scaled (4 MiB) RAM profile from one harness
  (`docs/src/architecture/memory.md` §7s). The caps and band watermarks stay
  implementation constants, never ABI.
- SWAP5 (optional encrypted lower-tier block swap) is now fully designed as
  **partition swap** in `plans/FIX-SWAPFILE.md` (planned, SF0–SF6): a dedicated
  raw block partition — never a file in an ARXFS volume — page-slotted with
  short compressed writes, its own ephemeral `SealKey`/`NonceSequence`,
  compress-once re-seal on the `ramzip → partition` demotion, no durability
  redundancy but mandatory AEAD integrity detection (corruption fails closed to
  a killed task + audit), `swapon`/`swapoff` over multiple durable-identity
  block devices with a draining `swapoff`, removable default-off, and
  per-user/per-task swap quotas via the §24.3 rlimit facility. It supersedes the
  one-paragraph SWAP5 sketch in `plans/SWAPSWAPSWAP.md` §15.
- Per-page latency measured on real boards under real workloads remains
  future work alongside SWAP5, where such workloads exist to measure; the
  host suite above supplies the pre-hardware evidence.

---

## CAPABILITY_USE — the capability lifecycle: login → session → administration (`plans/CAPABILITY_USE.md`)

**Status: CU1–CU7 done.** The full capability lifecycle is
wired: the kernel computes *effective = user grant ∩ manifest request*
(`TaskCapabilities::derive`), the runtime spawn path threads the account's
`capability_grants` ceiling through `SpawnCredential` into that intersection
(inherit copies the caller's stored ceiling; a `CAP_SPAWN_AS_USER` switch
resolves the target account's — CU1), every embedded program carries a
pinned manifest sized to every gated code path it can exercise — including
capability-gated optional features that degrade gracefully when the
intersection strips them (`top`/`ps`/`sysinfo` request the privileged
`CAP_SYSINFO_*` queries their optional features issue; the above-baseline
subset per session tool is pinned as its own audited set — CU7) — with the
shell on its own exercised set (`SHELL_MANIFEST`; the account baseline
additionally carries the graphical-session class
`CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM` — CU2/CU6),
the debug root account is seeded with the shared `administrator_ceiling()`
and the whole flow is proven by the `session_ceiling` QEMU vertical (CU3),
and a running system administers its accounts through the
`CAP_USER_ADMIN`-gated, audited `users_admin` syscall — a kernel engine
enforcing never-widen grant editing, the last-administrator guard, full
re-validation, crash-safe persistence to the encrypted root, and
next-spawn/next-login binding, with the interactive `users` tool as its
first holder (CU4), and per-invocation elevation is live: the shell's
`elevate <user> <program>` builtin posts to its console's login supervisor
over the reserved per-console rendezvous (`lib/abi/src/elevate.rs`), which
re-authenticates the target account and spawn-as-user runs the program
while the shell blocks — backed by the `WaitSourceKind::Child` wait-set
member, the kernel-attested `Origin::console`, and the
`is_reserved_endpoint` bind gate that keeps squatters off well-known
service endpoints (CU5). The plan binds: where each principal's ceiling
comes from (system programs = manifest; user sessions = the account grant
snapshotted at the switch and inherited by every descendant), the
interactive **session baseline**, the **administrator** as a grant set (no
uid-0 power, no wheel group), spawn as the only delegation point (narrowing
only), next-spawn revocation, and elevation as re-authenticated
spawn-as-user through the session service — never setuid or a runtime raise.
CU6 is complete: the session/ceiling slice landed with `plans/DISPLAY.md`
D7d and the picker-issued one-shot descriptors with `plans/APPWIN.md` AW5
(the kernel `fd_grant`/`fd_redeem` read-only delegation redeemed by a
consumer holding no filesystem capability). Remaining: the installer
first-user flow (with the installer work). See `plans/CAPABILITY_USE.md`
for the binding design and staging.

---

## UNIVERSAL — universal app distribution: multi-slice bundles + a Wasm app tier (`plans/UNIVERSAL.md`)

**Status: planned (U1–U2).** One published artifact runs on every TAIRiX
architecture: the **`.app` bundle** (never a fat `rxe` file) is the universal
unit. U1 (small, first) adds the target-architecture dimension to `AppInfo`
and per-arch slice selection to the single `lib/appload` load gate, with
install-time thinning of foreign slices — native, per-arch slices remain the
only correct answer for hot-path code (§2.16). U2 (large, second) adds an
optional `wasm32` fallback slice compiled by a sandboxed (§19.5) install-time
AOT service under `CAP_JIT_MAP_EXEC` W^X discipline, so any architecture
without a native slice — including ones added after publication — runs the
existing catalogue. Explicitly never built: a bespoke bytecode/VM, a fat
`rxe` file format, a default in-process runtime JIT, or an in-OS Rust
compiler as the distribution channel. See `plans/UNIVERSAL.md` for the
binding design and staging.

---

## SYSCONFIG — boot machine facts + the boot-time configuration store

**Status: done** (both deliverables shipped; the registry grows by adding a
`Key` variant to `lib/sysconfig`, never a second store).

- **Boot facts (`boot_facts_get`, syscall 89) + the machine-summary
  banner.** The kernel mints one immutable `tairix_abi::BootFacts` record at
  boot — the arch port's stated identity (`KernelArch::arch_id`, `None` on
  the host test arch so the facts stay uninstalled), the boot CPU's
  discovered model name (`KernelArch::cpu_name` → `tairix_abi::CpuName`:
  the x86_64 CPUID brand string, the aarch64 `MIDR_EL1` decode, the riscv64
  device-tree cpu `compatible` mapping; `CpuName::UNKNOWN` when none is
  derivable), the validated CPU
  count, and the boot path's pre-carve installed-RAM total
  (`BootInfo::with_installed_memory`; aarch64 sums the raw FDT `/memory`
  windows, riscv64 takes the FDT window, x86_64 sums the firmware map's
  usable RAM before the kernel-image carve) — and serves it through the
  ungated, un-audited `boot_facts_get` syscall (the `boot_id_get` shape: the
  machine's public shape, never live state; live figures stay behind the
  capability-gated System Information API). PID 1 renders its startup banner
  from it: `TAIRiX <version>: <mem>` (whole MiB rounded to nearest; whole
  GiB above 100 GiB), a blank line, then `<CPU name>, <n> core(s)` — e.g.
  `ARM Cortex-A72, 4 cores` — falling back to
  `Unknown <arch> processor, <n> core(s)` when no model was discovered; a
  kernel with no installed facts
  degrades the banner to the version line, fail closed, with the reason on
  stderr. Wrappers: `tairix_rt::boot_facts()`, `tairix_sys_boot_facts_get`.
- **The boot-time configuration store (`lib/sysconfig` + the `configure`
  command app).** `/System/Settings/Configuration/system.conf` on the
  encrypted root is the one administrator-settable boot-time store: a
  bounded `key value` line grammar with a **closed** key registry
  (`os.loginType` = `text` | `graphical` today), fail-closed parse, and
  canonical render, all defined once in `lib/sysconfig` (no_std+alloc,
  host-tested). The `configure` command app (`userland/apps/configure`,
  `CAP_CONSOLE_WRITE`+`CAP_FS_ACCESS` — write authority is the
  `/System/Settings` per-inode policy, no new capability) lists/shows/sets
  through that engine and refuses an unknown key, an out-of-set value, or a
  malformed store outright. Consumer: `login` re-reads the store each round
  (post-unlock by construction — the store lives on the encrypted root) and
  a configured `graphical` default starts the desktop directly after
  authentication when one is available, degrading to text otherwise — never
  an error. Remaining: nothing for the shipped keys; new settings enter by
  extending the `Key` registry and its consumer in the same change.

## Cache-Aware Scheduling (LLC-aware task aggregation)

**Status: planned.** A scheduler *performance* feature (§2.16): co-locate the
threads of a process that share data onto the same Last-Level-Cache (LLC)
domain so they hit a warm shared cache instead of bouncing cache lines across
LLCs. On a machine with more than one LLC the cross-LLC miss penalty is real
and measurable; upstream Linux's cache-aware load balancing (merged for
Linux 7.2) reports double-digit gains on multi-LLC parts (e.g. AMD Zen
CCX/CCD, Intel multi-tile / sub-NUMA, and clustered ARM/RISC-V server SoCs).
TAIRiX supports such parts, so this is worth carrying — but only as a measured,
default-safe improvement, never a guess (§2.16: measure, do not guess).

**Design decisions (binding):**

- **It is a `SchedulerPolicy` concern, not a kernel-wide one (§17.1).**
  Cache-aware aggregation is a load-balancing *policy* behaviour. It is
  expressed through the existing `kernel/sched/api` contract (a per-policy
  capability surfaced on `SchedulerPolicy` / driven through `SchedulerArch`),
  implemented by each concrete sibling policy that opts in
  (`kernel/sched/eevdf`, `kernel/sched/mlfq`), and exercised by the shared
  `kernel/sched/api/tests` conformance suite. No crate outside `kernel/sched/*`
  / `kernel/core` learns a concrete policy or that this feature exists.
- **Topology is discovered, never compiled in (§2.20, §18).** LLC/cache
  topology — which CPUs share which LLC, and the LLC size — is added to the
  architecture-neutral hardware tree (`lib/abi/src/hwtree.rs`) and populated by
  each `kernel/arch/<target>` discoverer from its native source (x86_64 ACPI
  PPTT + CPUID cache leaves, aarch64/riscv64 device-tree `cache`/`next-level-
  cache` nodes via `lib/fdt`). The scheduler reads the normalised topology
  threaded through `SchedulerArch` (alongside the existing `CoreClass`); it
  never names a board, an SoC, or an MMIO base. wasm32 has no LLC topology and
  the feature is a no-op there.
- **`abi-v1` is *not* frozen** (the standing task direction supersedes the
  `AGENTS.md`/`PLAN.md` "frozen" language). Extending `hwtree` and any sched
  ABI is done **in place** (§2.13) — no `v2`-beside-`v1`, no shim — and the
  generated C header is regenerated (`cargo xtask c-header --write`, drift
  guard in `ci`).
- **Default-safe, regression-guarded (§2.16).** Aggregation runs on the load
  balancer (amortised, off the hot pick/wake path), gated by a working-set
  vs LLC-size check so a process whose footprint exceeds the LLC, or that
  spawns many non-sharing threads, is *not* over-aggregated (the v3→v4
  regression class upstream fixed). Two independently toggleable behaviours,
  mirroring the upstream split:
    - cache-aware **load balancing** — the cheaper path, eligible to default on;
    - cache-aware **wakeup** placement — more expensive, default off.
  A measured regression on any benchmark is a defect, fixed or the feature
  left off by default until it is not (§2.16, §2.18) — never shipped as a
  "for now" win (§2.19).
- **Security/correctness unchanged.** Placement is a hint only: it never
  weakens isolation, capability checks, fairness, or the no-starvation bound
  the conformance suite asserts (§17.1, §5.4). Fail closed — absent or partial
  topology falls back to the existing placement, never to a crash (§2.9).

**Deliverables:**
- `lib/abi/src/hwtree.rs`: LLC/cache-domain topology nodes (CPU→LLC mapping +
  LLC size), versioned/hashed like the rest of the tree (§18.1); C header
  regenerated.
- Per-arch discovery populating it (`kernel/arch/{x86_64,aarch64,riscv64}`);
  wasm32 reports none.
- `kernel/sched/api`: the per-policy aggregation hook + the topology-reading
  surface on `SchedulerArch`; conformance cases (aggregation honoured,
  working-set guard respected, no fairness/starvation regression, ≥ 4-core SMP
  with ≥ 2 LLCs).
- `kernel/sched/eevdf` (and `mlfq` where it applies) implementing the hook.
- A `sysinfo` (§16.6) read-only view of the discovered LLC topology behind the
  existing privileged hardware query — no `/proc`/`/sys` (§16.1).

**Tests:** host-side policy/conformance tests modelling a multi-LLC machine
(aggregation, working-set guard, fairness preserved); a QEMU vertical with an
emulated multi-LLC topology; a benchmark/measurement establishing the
default-on/off decision per behaviour (§2.16).

**Docs:** `docs/src/architecture/scheduler.md` (the aggregation model + the two
toggles + the working-set guard) and the `kernel/sched/*` rustdoc. The
`README.md` feature matrix carries the "Cache-aware scheduling (LLC-aware)"
row as planned (`▢` on the three bare-metal targets, `—` on wasm32); its
per-target marks are promoted in the same change that lands each port's
discovery + policy support (§13).

---

## Assignment Notes for Task Dispatchers

When handing a stage to an implementing agent, the task brief **must**:

1. Reference this `PLAN.md` and the `AGENTS.md` charter explicitly.
2. List the stage's deliverables, tests, and docs verbatim.
3. State the dependencies that are already satisfied.
4. Forbid stubs, `todo!()`, ignored tests, and `#[allow(...)]` without
   justification.
5. Require the agent to quote actual `cargo xtask test` output on completion.
6. Require the agent to apply the `AGENTS.md` §23 Code Review and Acceptance
   Gate to its own diff and state the §23.5 verdict on completion.

A stage delivered without the above is to be returned for rework, regardless
of how much code was produced.

---

## Charter Amendments

Amendments to `AGENTS.md` (the binding charter) are logged here so an agent
can see *why* a rule exists without diffing the charter's history.

- **2026-08-05 — Comments are terse, and a file's existing prose is not a
  precedent.** Amended §2.11 (and its §15.17 agent mirror and the §23.2 review
  bullet) after repeated drift where an agent found pages of exposition in a
  source file and matched that style, treating local convention as a licence to
  ignore the charter: comments now carry the required *why* and stop, prefer no
  comment where the code reads clearly, and the bar binds in every file however
  much prose is already there — "I matched the surrounding style" is explicitly
  not a defence. Waffle already in the tree is the `plans/WAFFLE.md` backlog to
  clear, never a style to copy.

- **2026-08-04 — Two system program stores and a fixed lookup order.**
  Amended §16.2/§16.3/§16.5 and added §16.8 (owner decision, `plans/APPS.md`
  §8): `/System` holds the OS-provided command apps in `Commands/` and the
  OS-provided graphical applications in `Applications/`, each user's home
  holds the same pair, and a bare command word resolves against the fixed,
  non-overridable prefix `/System/Commands`, `/System/Applications`,
  `<home>/Commands`, `<home>/Applications` before the user's `PATH`. Splitting
  the store by what a program *is* keeps the coreutils command set (§16.7) one
  reviewable, GNU-comparable surface instead of mixing it with desktop
  applications, and building the prefix from the store definitions rather than
  reading it from the environment makes "a system command cannot be shadowed"
  a structural property rather than a `PATH` convention. A bundle's own signed
  manifest kind (`command`/`application`/`service`) is the single place its
  store is decided, so no list of "which programs are commands" can exist to
  drift (§2.2). No new capability guards either store — the existing
  file-access and app-load gates apply (§5.2 minimalism).

- **2026-08-03 — An app's own icon is mandatory, and SVG is the preferred
  form.** Amended §10 (owner decision) to say plainly what the previous
  amendment left implicit: every command-line and graphical app **must** ship
  its own icon inside its bundle, and it is authored as SVG wherever the
  artwork can be expressed in the supported subset — one vector file then
  serves every slot and UI scale exactly — with a raster master reserved for
  an app whose identity is a rendered picture. Without the mandate an app
  could legitimately ship no icon and resolve to the one generic application
  picture, which is what made a store of fifty programs look like fifty copies
  of the same one; without the format sentence §10 read as *requiring* PNG for
  an application icon, so a vector master was refused by the image build even
  though the sandboxed rasteriser has always decoded SVG. The build now sniffs
  the format from the bytes exactly as the runtime does and refuses a declared
  icon it could not draw — absent, over-long, undecodable, a non-square or
  undersized raster master, or one that draws nothing — so a broken icon fails
  the build instead of degrading silently to a glyph. The rule is
  `plans/APPS.md` §14; the pipeline stays `plans/ICONS.md`.

- **2026-08-02 — Raster masters are a canonical source for illustrative icon
  artwork.** Amended §10 (owner decision) after the desktop's application,
  file-class, and device icons arrived as rendered pictures with no vector
  equivalent: "SVG-first for *every* WM/desktop asset" would have discarded
  them or demanded a fake SVG wrapping a bitmap. §10 now splits by what the
  artwork *is* — SVG stays canonical for tintable chrome (cursors, status
  glyphs, window furniture), while illustrative icons are authored as
  high-resolution square straight-alpha PNG masters that only ever downscale
  — and makes resolution explicitly total and three-tier: raster artwork,
  else the on-disk vector asset, else a **mandatory** first-party built-in
  glyph, so no icon can exist as a raster asset alone and no missing or
  corrupt file can blank a surface. The decode-once-per-(asset, side), one
  blend path, sandboxed-and-bounded rules are unchanged and now bind a system
  asset exactly as they bind a third-party bundle's icon. §3 and §16.2 were
  updated to say where the masters live.

- **2026-07-28 — Arch HAL `MachineTakeover` slice (optional).** Amended §17.2
  to enumerate a new closed Arch HAL slice, `MachineTakeover`
  (`kernel/arch/api/src/takeover.rs`), the irreducibly per-architecture
  mechanism the pre-boot Supervisor's one-way destructive whole-RAM test
  (`memtest full`, `plans/NEW-SUPERVISOR.md` §9) drives to own the machine: a
  single `take_over(sweep)` operation that owns the whole irreversible sequence
  (quiesce every other CPU, mask interrupts, stop the watchdog, flatten/relocate
  paging, switch to a reserved stack, run the caller's sweep, test the region it
  ran from, and reset) — one op because the sweep destroys the caller's own
  stack, so a "prepare then let the caller sweep + reset" split could never be
  correct. It is fail-closed (`TakeoverError`) and non-panicking, and
  enumerated as **optional** — unlike the mandatory slices, a port that has not
  wired it fails closed to "not supported" (`KernelArch::machine_takeover`
  defaults to `None`) rather than blocking the boot floor, so `wasm32` (a
  sandbox owning no physical RAM) simply declines. The `KernelArch::machine_takeover`
  accessor is **supervisor-gated** — it requires a
  `kernel/core::supervisor_system::TakeoverGrant` witness only the confirmed
  `memtest full` path can mint — so the destructive mechanism is reachable from
  nowhere else. Complete on all four Tier-1 targets: the arch-neutral trait +
  conformance + supervisor-gated `KernelArch` seam, the Stage-C `memtest full`
  command, the Stage-D fullscreen UI, and the real per-port bodies + Stage-E
  destructive QEMU verticals for riscv64, aarch64, and x86_64 (wasm32 stays
  `NotSupported`).

- **2026-07-27 — Arch HAL `CoreClock` live-frequency slice.** Amended §17.2 to
  enumerate a new closed Arch HAL slice, `CoreClock`, after the System
  Information API needed to report each CPU's *live* clock speed and the
  existing `CpuCycles` counter turned out to be a fixed-rate *reference* clock
  (`CNTVCT`/`time` CSR/invariant TSC) that cannot track DVFS. `CoreClock` adds
  a genuine core-clock counter per port (aarch64 `PMCCNTR_EL0`, x86_64
  `APERF`/`MPERF`, riscv64 `rdcycle`; wasm32/host honestly unsupported) over
  the fixed reference; the kernel's per-CPU estimator divides the two deltas
  (`Δcore·ref_hz/Δref`) at the preemption tick to publish an honest measured
  frequency, never a fabricated one.

- **2026-07-21 — First-party Rust boot chain.** Amended §3 (added `lib/bootload`,
  `lib/multiboot2`, and the planned `boot/` per-firmware shells), §12 (a
  boot-chain note), and the §15.18 jump-sheet (a `plans/BOOTLOADER.md` row),
  after the x86_64 product image
  was found to have no way to boot on real firmware without GRUB (forbidden C /
  external code). The decision: a first-party, Rust-only loader — the pure
  `lib/bootload` core plus per-firmware `boot/*` shells — handing off through the
  kernel's existing multiboot2 entry, so no third boot protocol is invented.
  `plans/BOOTLOADER.md` is the staged design; `plans/ARCHSUPPORT.md` A1 depends
  on its GPT/ESP whole-disk builder (B4).

- **2026-07-20 — "Machine load" is never an excuse for a flaky test.**
  Strengthened §7's "No flaky tests" clause and added a §23.4 gate bullet
  (maintainer request) after failures were repeatedly dismissed as "flaky
  because of machine load, so I re-ran it alone and it passed" — a get-out that
  every time masked a *real* defect the load merely exposed (a race, an
  unsynchronised wait, an unbounded queue, a budget sized to an idle host, a
  missing completion signal). The rule now names and forbids that get-out in
  every phrasing (machine load, CPU contention, oversubscribed host, slow
  runner, "passes when run on its own"), states that re-running a failed test
  in isolation is neither an investigation nor a fix, and forbids closing work
  while any test has failed even once. Mirrored in `docs/src/contributing.md`
  (new "Flaky tests are defects" section), `tools/ci/README.md` (the soak /
  QEMU-timeout framing: a load-dependent timeout is a defect fixed
  structurally, not niced or re-run away), and `plans/CODEVERIFY.md`. Charter +
  docs only; no code changed.

- **2026-07-15 — CFQ: the one sanctioned non-tickless scheduler, now the
  default.** Amended §17.1 (maintainer request) to grant a single, explicit
  exception to the tickless (NO_HZ) mandate: the new `kernel/sched/cfq`
  policy — a Linux-CFS-like Completely-Fair-Queuing scheduler — keeps a
  fixed-frequency periodic quantum tick armed for *any* running task,
  including a lone CPU-bound one (exactly the case the tickless rule forbids
  arming for), so every task is periodically preempted like Linux's `HZ`
  tick. This is deliberate and granted to CFQ **alone**; EEVDF and MLFQ stay
  fully tickless. CFQ is made the default `scheduler-*` feature in
  `kernel/core`; EEVDF/MLFQ remain selectable. Motivation: a Linux-familiar,
  always-preempting default that never leaves a sole CPU-bound task without
  an armed quantum timer (the `stress --cpu 1` responsiveness class).

- **2026-07-08 — The `plans/` jump-sheet.** Added §15.18 and repointed the
  §3 `plans/` comment at it (maintainer request), after capability work was
  done without consulting `plans/CAPABILITY_USE.md`: agents (and humans)
  had no single place telling them which binding plan governs an area, so
  staged designs risked being silently re-derived or contradicted. §15.18
  is a topic → plan table every contributor checks before touching a
  covered area, maintained in the same change that adds or removes a plan.

- **2026-07-07 — A user's own stores, nested bundle filing, and `man`'s
  recursive help search.** Amended §16.3 (maintainer request): the fixed
  user-home shape carries the user's own program stores, and bundles in
  `/Apps` or a user's own store may be filed in nested plain subdirectories.
  `man` resolves a bare word through the ordered lookup candidates first and
  then falls back to a bounded, breadth-first recursive walk of `/Apps` then
  the user's own stores (never descending into a `.app` — a bundle is a
  sealed unit), so `man moose` finds `/Apps/somefolder/moose.app`'s help
  wherever it was filed. The roots are spelled once in
  `lib/cmdres::search_roots` over `tairix_abi::INSTALLED_APP_STORE`; the walk
  fails loud when its directory budget is exhausted rather than masquerading
  as "not found". `/Apps` stays off the launch path: the shell searches the
  §16.8 fixed prefix and `PATH` only.

- **2026-07-06 — Foundational implementations are complete, not minimal.**
  Added §27 (maintainer decision) after kernel building blocks were found
  implemented as the thinnest slice their first caller needed rather than
  the real primitive — e.g. `kernel/core/src/waitq.rs`: an O(n) `Vec` of
  waiters with `wake_all` as the only wake path (no wake-one, no FIFO/
  priority, no fairness/anti-starvation, O(n) `register`/`deregister`/
  `sweep`/`earliest_deadline`). §2.3/§2.4 (no bloat / no creep) had been
  read as licence to ship an incomplete core; §27 resolves the tension:
  a foundational primitive is built as the complete, production-grade
  abstraction it names, with the data structure/algorithm a real kernel
  would use and a stated fairness/ordering discipline, while still adding
  no speculative surface. "Minimal for now" is the §2.19 defect; too-large
  work is landed complete-as-far-as-it-goes and escalated (§15.7), never
  shipped thin. The `waitq.rs` rework itself is too large for this charter
  change, so it is staged and surfaced as PLAN **P-6** (§2.18/§2.19/§15.7)
  rather than left silent. The rework has since landed complete as PLAN
  P-6 (an O(log n) three-index wait set with a stated FIFO no-starvation
  discipline; see P-6).

- **2026-07-05 — Fail loud, degrade gracefully.** Added §2.24 (maintainer
  decision) after `top` was found exiting silently (code 1, no message) when
  the `a` key's system-wide view was refused for want of
  `CAP_SYSINFO_GLOBAL`. Two duties: every abnormal exit states its reason on
  `stderr` (from the program, or from the spawner/shell when the program
  cannot), and a refused *optional* action is reported (UI/status line or
  `stderr`) and survived with the authority the program has — the action
  fails closed (§5.4), the session does not die over it. Only a failure of
  the program's primary purpose is fatal, and then with its reason stated.
  Extended 2026-07-07 (maintainer decision): where no terminal/stderr
  consumer can show the reason to a user, the observing component records
  the termination through the system log (`lib/log`) instead — prompted by
  the `tairix-rt` panic handler, which discarded its `PanicInfo` and exited
  silently.

- **2026-07-05 — System command apps follow GNU coreutils.** Added §16.7
  (maintainer decision): the OS-provided command apps (`ls`, `cat`, `cp`,
  `mv`, `rm`, `ps`, `top`, …) match GNU coreutils option names, argument
  grammar, and default output as closely as possible, so a user or script that
  knows the GNU tool finds ours familiar — the burden of proof is on any
  deviation. TAIRiX-native concepts (capabilities §5.2, the storage forest
  §16.1, `Time64` §21, the System Information API §16.6 instead of a fabricated
  `/proc`) diverge deliberately and only where they genuinely differ, and the
  `stdinfo` stream (§20) is additive on fd 3 — never a reshaping of the
  coreutils-compatible stdout/stderr. Security and correctness (§5.4, §4)
  still win over bug-for-bug fidelity. The per-command specifications stay in
  `plans/APPS.md`; §16.7 binds the principle.

- **2026-07-04 — Services are apps.** Amended §16.2 (maintainer decision,
  `plans/APPS.md` deliverable 8): a long-running system service under
  `/System/Services/` is not a special program class — it ships as the same
  self-contained, signed `<name>.app` bundle §16.5 binds every app to,
  discovered from disk and loaded through the identical signature +
  capability + interface-hash gate. A second, weaker "service" packaging
  format would be a second trust path (§2.2); the only compiled-in program
  is PID 1 `init`, which the boot path enters before any volume is mounted
  (the §18.6 boot floor).

- **2026-07-04 — App bundles are self-contained; no app code baked into the
  kernel.** Amended §16.5 (and the §16.2 program-store note) after command
  apps (`ls`, `ps`, `man`) were found with only `Help/` on disk while their
  `Run` rxe was compiled into the kernel and dispatched by a byte-exact
  in-kernel spawn-path lookup — so browsing `ls.app/` showed only `Help/` and
  bypassed the `appmgr` verification path. The rule now binds an app as *its
  bundle directory*: every part (Run, Code/, AppInfo, Resources/,
  DefaultSettings/, Help/, app-private static/shared libs) is a real file
  inside `<Name>.app/`; app code is never compiled into or served from the
  kernel/image builder, and the store is discovered by scanning on-disk
  bundles, never a compiled-in registry (§2.2, §18.3/§18.6). The only outside
  reach is the curated `/System/Libraries/` set and the syscall ABI.

- **2026-07-04 — Command help is authored in the bundle, never hardcoded.**
  Amended §16.5 (`plans/APPS.md` §6.1) after `ls`/`man` were found embedding
  their own `Help/` trees via `include_bytes!` in a per-app `help.rs`, which
  two hand-maintained lists (in `tools/mkimage` and the QEMU fixture) then
  planted — so adding a bundle forced edits to central files, the duplication
  §2.2 forbids. The rule now binds help as *data on the volume*: authored once
  in the bundle's on-disk `Help/` tree, read at runtime through the `lib/help`
  seam, never `include_str!`/`include_bytes!`/baked into a program, and never
  planted from a per-bundle list. Added `tools/syshelp` to §3 — a build-time
  scan of the command-app bundles' own `Help/` sources — so the image builder
  and fixtures plant from discovered data; deleted the per-app `help.rs`
  copies and both mkimage/fixture lists (§2.14).

- **2026-07-03 — `lib/cmdres`: the shared command-word resolution policy.**
  Added to §3 (`plans/APPS.md` §8–§9): the pure store-then-`PATH` candidate
  policy moved out of the shell crate into its own `lib/*` crate so the
  `man` command's bundle lookup can import the identical order without a
  forbidden userland→userland dependency (§17.4) and without a second
  resolution policy (§2.2).

- **2026-07-03 — One bundle help tree: `Documentation/` merged into `Help/`.**
  Amended §16.5 (the merge alternative of `plans/APPS.md`, maintainer-chosen):
  the bundle's `Documentation/` entry is renamed to `Help/`, the
  internationalised structured-Markdown tree (one document per command/topic,
  one directory per BCP-47 locale with the mandatory canonical `en-US/`) that is
  the single source for `man`, short `-h`/`-?` help, and any graphical help
  viewer — two overlapping documentation entries would be the duplication §2.2
  forbids. In-place `abi-v1` evolution (§2.13): `BundleEntry::Help` replaces
  `Documentation` with every caller/fixture updated and the C header
  regenerated.

- **2026-07-01 — Storage is a forest of named roots; `/` is a view, not
  identity.** Amended §16.1 (Option B of the `plans/DRIVES.md` brief): the four
  top-level names become exactly four entries in the *default session root
  view*, backed by the first-class aliases `System:`/`Users:`/`Apps:`/
  `Storage:`, and the canonical identity of a storage root becomes its root ID
  (`id::`) or alias path, never the `/` view path — so a healthy volume stays
  reachable by `id::` when the `/` view or `System` volume is absent/corrupt.
  This removes the Unix single-root failure model while preserving the clean
  four-name user layout, and preserves §16.2/§16.3 as the aliases' policy. The
  binding model is the new storage-namespace spec (`docs/src/filesystem/
  drives.md`, prerequisite P4.1); the descriptor-producing open-a-path ABI and
  the `lib/path` resolver `Root` variants are the remaining, still-open P4 work.
  Documentation only (no code/interface changed yet).

- **2026-07-01 — "Transient" is not a diagnosis; a load-dependent timeout is a
  flake.** Hardened §7 "No flaky tests" after a QEMU test timeout was wrongly
  waved through as a "transient load flake" and reported done on a clean
  re-run: the rule now forbids reclassifying an intermittent failure as
  transient/load-flake/environment-blip to dodge the fix, states a green re-run
  is not a fix (it only proves the flake), and names a timeout reachable solo
  but missed under parallel load as a load-dependent flake needing a structural
  fix. Fixed the concrete instance: removed the developer-only 30 s
  `DEVELOPER_TIMEOUT_CAP` clamp in `tools/xtask` that halved each QEMU
  enrolment's reachable budget locally, so developer and CI now enforce one
  budget sized to the work; deleted the now-dead `in_github_actions` timeout
  signal (§2.14).

- **2026-06-30 — Legacy POSIX names: the OS never authors them, the kernel
  does not police the user.** Reworded §16.1: the ban is on the *OS* creating
  the reserved legacy top-level names (installer/image-builder/in-tree code),
  not a structural refusal the VFS imposes on userland. A user with write
  authority on `/` may create `/etc` like any other directory; ordinary
  owner/mode/ACL on the root governs it, with no new capability (§5.2) and no
  reserved error. Removed the VFS `ReservedPath` refusal and the now-dead
  `RESERVED_TOP_LEVEL`/`is_reserved_top_level`/`VfsError::ReservedPath` surface
  (§2.14); OS-side non-authoring stays (mkimage authors only the four,
  installer refuses per §11).

- **2026-06-24 — No charter-section citations in code comments.** Scoped
  §2.11's "references" to external/cross-file pointers (specs, manuals, papers,
  other in-tree files/plans) and forbade citing `AGENTS.md` section numbers
  (`§5.4`, `sec.5.4`, "Section 5.4", bare `(§2.9)`) in comments; the *why* is
  stated in prose ("the charter forbids …" where the charter is the subject).
  Added §15.17. Generator-stamped provenance (`include/` C-header banners) and
  runtime diagnostics that point at a violated rule are exempt. Stripped the
  citation tokens tree-wide (rationale prose retained); comment-only change, no
  interface or behaviour touched.

- **2026-06-07 — Code-quality & self-review hardening.** Added §2.13 (no
  pre-release backwards-compatibility code — TAIRiX has not shipped, so
  TAIRiX-native interfaces, types, and on-disk formats are evolved *in place*
  with all callers updated in the same change; no `v2`-beside-`v1`, shims,
  migrations, or "old data" fallbacks; this is distinct from reading *foreign*
  ext4/FAT32 volumes under §21 and from the §2.4 freeze that binds only from
  the first release). Added §2.14 (delete obsolete code — nothing commented
  out, `_old`-renamed, `#[allow(dead_code)]`-ed, or orphaned; deletions update
  §3 / §16.4 / this plan). Added §23 (Code Review and Acceptance Gate — a
  binding adversarial self-review every agent runs on its own output before
  reporting done: §23.1 security, §23.2 correctness/multi-arch, §23.3
  no-compat/no-dead-code, §23.4 tests/docs/process, §23.5 verdict), cross-
  referenced from §14 (mergeable criteria) and §15.12 (agent instructions).
  No code or interface changed; this amendment is documentation only.

- **2026-06-09 — Plan files are not changelogs.** Added a §13 rule: `PLAN.md`,
  `plans/*.md`, and any planning/status document state the *current* plan and
  state only (deliverables, decisions/invariants, status, remaining work) — git
  holds the history. Forbids per-increment landing logs, commit hashes/dated
  session entries, quoted CI output, and superseded/"historical" prose; a
  completed item's prose is *replaced* with a done-state summary (§2.14). This
  amendment is documentation only.

- **2026-06-09 — Fix every defect, caused or noticed.** Added §2.18
  generalizing §2.17's security "fix it now" to *every* defect via two explicit
  channels: (1) any failure the whole-project gate surfaces and (2) any defect
  noticed by reading/reasoning even with a green gate — both fixed in the same
  change, with stop-and-ask (§15.7) the only escape for genuinely large ones;
  "unrelated"/"pre-existing"/"out of scope"/"the gate didn't catch it" are not
  exits. Reinforced §7 and the §23 intro to match. Documentation only.

- **2026-06-09 — Every fixed bug always gets a regression test.** Closed the
  remaining nuance left by §2.18: the "write a test" duty was anchored to a
  fix and silent on escalated-but-unfixed defects. Added a §2.18 bullet, a §7
  bullet, and tightened §23.4 so every defect that is fixed lands with a
  fail-before/pass-after regression test (fuzzer/proptest finds also enter the
  corpus, §19.6) — there is no path that fixes a bug without its test, and an
  escalated defect (§15.7) carries the test requirement with it until the fix
  lands. Documentation only.

- **2026-06-10 — Resources must scale; no fixed-constant ceilings.** Added §24
  (Resource Limits and Scalability) after the fixed-2 MiB-stack-arena and
  fixed-`MAX_CPUS` review found a recurring scaling-cliff pattern: a resource
  *capacity* hard-wired as a `const` that caps a large machine and wastes a
  small one. §24 requires capacities to be derived from §18 discovered
  hardware and to grow on demand, with one default *policy* sensible for both
  desktop and server, a capability-gated (`CAP_RLIMIT_RAISE`) `ulimit`/`rlimit`
  equivalent for settable per-process/user limits, and a §24.4 carve-out
  keeping security/format *bounds* on untrusted input deliberately fixed and
  fail-closed (widening those is a §2.17 regression). Implemented as the PLAN
  §24 stage; the audited sweep of offending constants is now complete.
  Documentation only.

- **2026-06-16 — Ban "for now"; finish the dependency or escalate.** Added
  §2.19: a knowingly partial/temporary "for now" solution is forbidden — if the
  proper solution depends on unfinished prerequisite work, that prerequisite is
  done in the same change, and if doing it conflicts with another rule/design/
  requirement the User is told to decide (§15.7), never resolved with a self-
  chosen compromise. Reinforced as §15.13 (agent instruction). The positive
  form of §2.1/§2.17/§2.18: deferring *correctness* is a defect. Documentation
  only.

- **2026-06-16 — Generic/multi-arch code is platform-neutral.** Added §2.20:
  shared `lib/*`, arch-neutral `kernel/*`, the driver host and core driver
  *frameworks*, and `userland/*` must carry no board/SoC reference (Raspberry
  Pi, BCM, specific UART/GIC/MMIO base, `cfg(board)`); platform specifics live
  only in `kernel/arch/<target>/` (plus the §1 boot-stub carve-out) and reach
  every other layer at runtime via §18 discovery, with a carve-out for a
  concrete device's own driver/support crate reached only through the §18.3
  match path. Makes §17.2/§17.4 absolute for generated code; reinforced as
  §15.14. Documentation only.

- **2026-06-16 — Driver path namespace is class/bus-type, never vendor.**
  Clarified §8 (and mirrored in §16.2): every directory level above the leaf —
  the source `drivers/<class>/` path and the installed
  `/System/Drivers/<class>[_<subtype>]/` path — is named only by device class
  or bus type; a vendor/product name is permitted *only* as the leaf
  *directory* that holds the driver file(s) for that one part (the §2.20
  carve-out applied to naming). So
  `/System/Drivers/bus_usb/broadcom_chip_1234/<driver>` is correct, while
  `/System/Drivers/broadcom_usb/broadusb1234` (vendor as a namespace segment)
  is a defect. Documentation only.

- **2026-06-16 — No-duplication binds constants, not just logic.** Extended
  §2.2: a value that is the same across sibling files by definition (shared
  layout offset, stack size, address bias, capability set, magic number, table)
  is defined once and imported, never copy-pasted — prompted by the user-stack/
  MMIO-window/canary constants duplicated across `init_spawn.rs`,
  `init_spawn_riscv64.rs`, and `init_spawn_x86_64.rs`. A constant lives beside
  one implementation only when it is genuinely that implementation's own (an
  arch-specific register layout, a runtime-discovered per-board MMIO base), not
  a value that merely coincides today. Documentation only.

- **2026-06-16 — Driver *set* is discovered, not compiled in (two tiers).**
  Added §18.6 and tightened the §18 intro / §18.3 / §18.5: the set of loadable
  drivers is discovered at runtime by scanning the installed signed bundles
  under `/System/Drivers/`, since no build can enumerate every future bus/
  vendor/interface — adding hardware support is dropping a signed bundle, not a
  kernel recompile. The only compiled-in exception is the irreducible bootstrap
  floor (root-complex/bus bring-up + the storage path) that must exist before
  the store is reachable; it is per-entry justified, still binds by discovery-
  match through the one shared `lib/devmatch` policy, and is signed + capability-
  gated like every driver. A plain leaf driver in that floor (e.g. a HID
  keyboard) is a defect — it belongs in the discovered tier in user space.
  Documentation only.

- **2026-06-18 — Minimize arch-specific code; share across all archs.** Added
  §2.21: arch-specific code under `kernel/arch/<target>/` is a last resort,
  permitted only for what the silicon strictly makes target-divergent
  (registers, privileged instructions, MMU/TLB/context-switch, errata,
  discovery source); everything expressible over the Arch HAL (§17.2) and
  `lib/*` must be. Single-arch work must check the sibling ports and hoist
  identical logic into a shared home (`lib/*`, an arch-neutral `kernel/*`
  subsystem, or a `kernel/arch/api/` default) — values that differ only by
  runtime discovery (§18.1) are data, not arch-specific code — never leaving a
  common routine stranded in one arch's file to be re-derived later (§2.2,
  §2.19). Reinforced in §17.2, as agent instruction §15.15, and as a §23.2
  self-review check. Documentation only.

- **2026-06-20 — Tickless (NO_HZ) kernel is mandatory.** Added a §17.1 rule
  (operator decision, "option B"): no CPU may be driven by a fixed-frequency
  periodic timer interrupt — scheduler accounting, preemption, and timekeeping
  must all use a **one-shot** timer the scheduler arms to the next event it
  needs, left unarmed when a CPU is idle or runs a single runnable task. EEVDF
  is already fully tickless; the sole carve-out is a policy that genuinely needs
  periodic wakeups (MLFQ's anti-starvation boost), which must request its own
  on-demand one-shot wakeups and never reintroduce a global tick. The rule makes
  the P-1 100 Hz fixed-frequency periodic preemption timer a charter defect; its
  migration to one-shot/scheduler-armed form is too large (all three bare-metal
  ports + Pi metal re-confirm) to land in this charter change, so it is staged
  and surfaced as PLAN P-4 (§2.18/§2.19/§15.7) rather than left silent. Charter
  + plan + docs only; the timer-arming code change is P-4.

- **2026-06-23 — No cooperative dispatch loop; the kernel must be fully
  preemptive (operator decision).** Added a §17.1 rule: a port MUST NOT run its
  EL1/S-mode/M-mode dispatch loop (or any in-kernel task) with interrupts
  masked for the whole span a task/operation runs, taking interrupts only at
  voluntary yield points. Metal on the Pi 4 ground to a halt mid-line because a
  single in-kernel `pcie_brcm` MISC read stalls ~4.3 s with IRQs masked, and on
  this cooperative, effectively single-CPU loop *nothing else ran* — not the
  preemption timer, not the buffered serial drain (§20), not the keyboard pump
  or login. Interrupts (the preemption tick and device IRQs alike) must be
  *deliverable while in-kernel code runs*; non-preemptibility (§4) is enforced
  **narrowly**, masking IRQs only around the genuine critical section
  (run-queue/context-switch, a held `lib/sync` lock), never across a whole task
  run. This is the structural cause behind the long serial-stall saga (the
  buffered-serial work treated the symptom). The kernel-wide rework — make EL1
  run with device interrupts enabled, audit lock discipline so an IRQ taken
  mid-task cannot deadlock, and prove the §17.1 CPU-bound-task preemption
  conformance on metal — is too large for this charter change, so it is staged
  and surfaced as PLAN **P-5** (§2.18/§2.19/§15.7), the immediate next work.
  Charter + plan + next-pi-prompt only; no code changed in this amendment.

- **2026-06-23 — AI agents may not commit or push to git.** Added a §14 rule
  and agent instruction §15.16 forbidding an AI agent from running `git commit`,
  `git push`, or any commit-writing/remote-publishing command (amend, rebase,
  merge, tag, force-push, or git aliases/hooks/scripts that do so). Creating
  commits and pushing are reserved for the human contributor who reviews the
  agent's working-tree changes first; the agent's deliverable is the modified
  tree plus its §23.5 completion report. Documentation only.

- **2026-06-24 — Device-driver logic lives in the driver, not `lib/*`.** Added
  §2.22 (cross-referenced from the §2.20 carve-out and §17.4): a `lib/*`
  device-support crate is permitted *only* when a charter-legal **non-driver**
  consumer shares it (a §18.6 bootstrap-floor path, or a driver of a *different*
  class); a transitional in-kernel scaffold is **not** a valid second consumer
  — it is the §18.5 defect to remove. A device-support crate that loses its
  last non-driver consumer collapses into its single `drivers/*` consumer as a
  host-testable `lib` target the `Run` binary links (§2.2, §2.14). Applied the
  rule: with the in-kernel keyboard scaffold gone (D5d), `lib/vl805` and
  `lib/pcie_brcm` had only their sibling driver bins as consumers, so both were
  folded into `drivers/bus/usb/vl805` and `drivers/bus/pcie_brcm` and deleted;
  `lib/vcmailbox` stays (its non-driver consumer, the aarch64 framebuffer boot
  console, is genuine). Charter + §3/§16.4 + docs + the fold.

- **2026-06-29 — Huge drives on a tiny machine (combined minimum floor).**
  Added §26.7 making the §26 operating-conditions hold *simultaneously*: a
  machine with as little as 1 GiB of RAM MUST mount and serve *several* 100 TB+
  drives at once without panic, OOM crash, mount refusal, busy-spin, or
  silent-corruption shortcut — the explicit conjunction of §26.1 (many
  heterogeneous disks), §26.3 (memory pressure), and §26.6 (very large
  filesystems). Binds resident metadata across all mounts to a working-set-sized,
  growable capacity (§24.1) rather than anything proportional to aggregate device
  size or volume count, keeps aggregate I/O fair/bounded (§24.3), and requires the
  §24.5/§7 scalability tests to exercise the conjunction, not each condition in
  isolation. Documentation only.

- **2026-06-30 — Capabilities stay minimal; a new `CAP_*` is a last resort.**
  Added a §5.2 rule (operator decision): the capability set is deliberately
  small, and a new capability is added only when it guards a real security
  boundary for a *group* of resources (never a single file/path/method — that
  is the per-inode `required_cap` model's job), has a live holder *and*
  enforcement point in the *same* change (no speculative "for later" caps,
  §2.3/§2.4), and is not already expressible by an existing capability;
  renaming/merging/deleting caps is in-place pre-release evolution (§2.13).
  Prompted by the PREREQUISITES.md P-B writable-`/System/Logs` work: rather than
  pre-define `CAP_LOG_WRITE`/`CAP_SETTINGS_WRITE`/`CAP_LOG_ROTATE` ahead of any
  journal/settings service, P-B adds **zero** new caps and gates the writable
  subtree by the existing inode/mount-flag/`CAP_FS_ACCESS` controls; the named
  caps arrive with their owning service. §16.2 softened to match. Documentation
  only.

- **2026-07-20 — ISR-shared locks must be interrupt-safe (review rule).**
  Added a §23.2 self-review bullet and a `lib/sync` `irq` rustdoc note: because
  syscall bodies and in-kernel tasks now run with device interrupts enabled
  (the P-5b syscall-entry work, `plans/FIX-SYSCALL.md`), any lock shared
  between an ISR and a syscall-reachable path MUST be an `IrqSafeSpinLock` or
  the ISR side must be lock-free with a deferred drain — a plain `SpinLock`
  shared with an ISR is a single-CPU self-deadlock. Documentation only.

## FIX-WILD — Debuggable user-fault kills (`plans/FIX-WILD.md`)  **[DONE]**

The user-fault kill path is now fully debuggable, at zero running-program
cost (all work runs only on a task already dying). Delivered:

- **Stage 1** (already landed): `AuditEvent::TaskFaultKilled` carries the
  kernel-attested `name`/`proc_id`, the `write` flag, and a coarse
  non-leaking `fault_offset` locality bucket.
- **`UserRegisterFrame` ABI** (`tairix_arch_api::backtrace`): the
  self-describing faulting-register frame (snapshot + `FrameLayout` +
  honest `fp_valid`), threaded by `*const` through the user-fault resolver
  on every port — `UserFaultResolveFn`, `DispatchHook::resolve_user_fault`,
  `dispatch_core::resolve_user_fault_via_slot`, and the integration test
  kernels — as one atomic 4-target change. Each port builds it from the
  register state it already saves at trap entry; **riscv64's `trap.s` was
  extended to persist the callee-saved set (incl. `s0`=fp)** so its
  fp-backtrace works too (frame grew to 256 B; offsets re-pinned by the
  `offset_of!` asserts).
- **PIE load base** recorded per task (`AddressSpaceRegistry::set_load_base`/
  `load_base`), so the crash `pc` and every frame are load-relative offsets.
- **Crash record + user-stack walk** (`kernel/core/src/crash.rs`): a
  bounded newest-first `CrashStore` and a `copy_in`-backed
  `UserStackReader: StackReader` over the *one* shared unwinder
  (`tairix_arch_api::backtrace::walk`) — a corrupt/unmapped user fp ends
  the walk cleanly, never faulting the kernel. `record_fault_exit` builds
  the record allocation-free from the threaded register frame.
- **`SysinfoQueryId::CRASH_RECORD` (id 20)** + `IntrospectDomain::Crashes`
  (id 15) + `CrashRecord`/`CrashNamedReg`/`CrashFaultClass`/
  `CrashFaultBucket`/`CrashRecordRequest` in `lib/abi/src/sysinfo.rs`,
  served from the kernel crash store (like the seat/IRQ domains) and
  brokered by `sysinfod`. **Gated on the existing `CAP_SYSINFO_KERNEL`**
  (no new capability, §5.2): it is the sole datum carrying absolute
  register values, so it matches the kernel-oops privilege boundary and
  never touches the shared audit log. C headers regenerated
  (`cargo xtask c-header`).
- **Stage 3 breadcrumb**: `elsh` states `shell: <name>: killed by fault
  (segmentation fault)` on `stderr` for a fault-killed child (status 139),
  keeping `$?` = 139, carrying no address/register/secret.

Leak policy held throughout: the audit log and the breadcrumb carry only
non-leaking cause classes/offsets; absolute register values live only in
the `CAP_SYSINFO_KERNEL`-gated crash record, and even there `pc`/frames are
load-relative. Docs: `docs/src/architecture/fault-diagnostics.md`.

---

## FIX-DESKTOP-SPEEDUP — desktop redraw speed without hardware acceleration (`plans/FIX-DESKTOP-SPEEDUP.md`)  **[A DONE, B DONE, C MOSTLY DONE, D DONE]**

**Dependencies:** Stage 7 (compositor, taskbar, controls). Independent of
`plans/FIX-DISPLAY-ACCELERATION.md` — that is the hardware half; this is
the software path, which stays the mandatory fallback on every target
(§17.3) and is what a backdrop-blur frame always takes.

**The defect that remains.** A pointer-motion sample over a control-rich
window still costs three whole-window passes above the compositor: the app
re-renders its whole surface, unpremultiplies and copies every pixel into the
shared frame, and the session converts and diffs every one of them. The
compositor itself is *not* the bottleneck here — `convert_damage` already
compares each presented pixel with what is there and reports only the
sub-rectangle that genuinely changed — so the waste is entirely upstream, and
closing it needs every control model change to report damage (C.1), not just
the pointer path that now does. A frame still presents up to eight
separate region round trips each copying a growing bounding box (E).

**Staged (detail in the plan; do not duplicate it here, §13):**

- **A — measure, and measure the right binary. [done, less the QEMU
  vertical]** `tairix-wm`/`-controls`/`-font`/
  `-window`/`-display` now build at `opt-level = 3` in the dev profile too
  (the debug/QEMU images build userland `Run` binaries there, so a
  measurement taken before this described the profile, not the code);
  `cargo xtask bench` drives the raster and whole-frame composite families
  through `lib/cpuops`'s `BenchHarness` over a host nanosecond counter; and
  `Compositor::frame_stats` reports exact per-frame work counts (damaged,
  blended, copied, frosted, encoded px, dirty rects, present calls,
  furniture-cache hits/misses taken as the delta of the cache's own
  accounting), surfaced as the Desktop block on the Switchboard's System →
  Resources page over the port that already carries the seat report — no new
  syscall, sysinfo query or capability, and a receiver that validates every
  count and fails closed. Still open: the QEMU hover vertical that gates on
  counter bounds.
- **B — stop blending the invisible. [done]** `WindowRow::opaque_run` +
  `compose_row` copy runs of genuinely opaque source pixels and skip every
  layer below them, so occlusion culling *is* the opaque-run path rather
  than a second per-window pass — finer, and sound without trusting a
  client's claim about its own content. `ChannelOrder::encode_run` in
  `lib/display` encodes a run in one call; it is not ABI surface (`lib/abi`
  cannot name a pixel type without closing the cycle `abi → raster →
  theme/reclaim → abi`). Bit-identity is proven by composing each scene
  twice, with the copy path on and off, and comparing bytes.
- **C — repaint the control, not the window. [C.0, C.2, C.4b, C.5 done; C.1
  partly; C.3 blocked; C.4a remains]** `tairix_geometry::Region` is the one
  region type (pairwise-disjoint band-canonical rectangles, a linear merge
  walk, an optional rectangle budget), and the WM's private copy is deleted;
  the compositor consumes it through a compose plan that promotes a
  backdrop-blurred window to one whole rectangle and *subtracts* it from the
  residual, so the frost cannot seam while unrelated damage stays tight.
  Containers route one hit test to at most the child left, the child entered
  and any child holding a press. `lib/controls/src/damage.rs` is the damage
  seam — one guarded write and a budgeted region — and the pointer path
  reports through it. Text measurement is memoised in `lib/font` beside the
  glyph cache, keyed by face and text and sharing the glyph cache's
  RAM-derived budget, with the monospace path paying no lookup. The shell
  settles once per drained input batch instead of once per sample (as do the
  keyboard and pinboard drains). Remaining: the keyboard/value control
  families and the old-bounds memory (C.1), the font hoist (C.4a), and
  — blocked until C.1 is complete — apps presenting real rects (C.3), which
  today would silently drop changes no reported rectangle covers.
- **D — blur costs what it changes. [D.1–D.4 done; D.5 is decision 2 below;
  D.6 is an unmeasured follow-up]** A frosted window's backdrop is retained
  (`userland/gui/wm/src/frost.rs`, `frost_cache`) because that blur is a
  function of the layers *beneath* the window and of nothing the window
  draws. Every `damage.add` in the compositor became one of three funnels —
  `mark` (a change not confined to one layer: the root fill, the desktop
  layer, the density or theme, a restack), `mark_layer` (a change confined to
  one window's own layer — its content, position, size, shape or furniture:
  drops only the frosts above *that* window) and `mark_overlay` (the cursor
  and the screen reveal: drops none) — because which frosts survive is exactly
  what the kind of change decides. That is what makes the two dominant
  interactions free: the pointer moving inside a frosted terminal, and a
  window dragged across one. A frost whose entry is still valid is no longer
  promoted to its whole rectangle, so a repaint inside one keeps the damage it
  marked; a recomputed one still is, and drops any overlapping frost above it.
  A retained entry records the window's **whole** rectangle, not the on-screen
  part of it, because a window pushed off an edge is frosted from where the
  screen begins while its shape is read from its own top-left. Whether a frost
  may be reused is asked **once per frame** and remembered, through the
  counted lookup, so a reuse reads as a cache hit and refreshes the entry's
  recency and the plan and the composite cannot read different answers. The
  cache is read-only for a whole composite pass and written at the end
  (`ReclaimCache::retain`, a new out-of-band admission that counts no second
  lookup and replaces what a key held), so admitting one cannot evict one the
  pass had decided to reuse. `lib/reclaim`'s `window_chrome_cache` was
  generalised to `screenful_ui_cache`, since its "no more of this can be
  visible at once than fills the screen" argument is the same for both. The
  box-blur mean is now a fixed-point reciprocal resolved once per pass instead
  of four divides per pixel per pass, *exactly* equal to the divide (the
  condition is checked for every window size in range, and that the cutoff is
  where it stops holding), and the sliding window's three strided walks are
  bounds-checked once per line. **Measured:** a `64×24` repaint inside a
  backdrop-blurred window **17.43 ms → 27.2 µs** (×640, with the pixels it
  touched falling from 564 000 to the 1 536 it marked), a full-screen re-frost
  **17.98 → 16.52 ns/px**, opaque cases unchanged. Bit-identity is proven by
  composing one scene twice — reusing frosts and blurring afresh — and by a
  naive `O(area·radius)` blur oracle.
- **E — one present per frame.** The disjoint damage region landed with C.0;
  what remains is a bounded rect *list* in one `Present` (same evolution as
  `plans/FIX-DISPLAY-ACCELERATION.md` Stage B), and one-shot tickless
  frame pacing (§17.1, never a periodic tick).
- **F — CPU-dispatched raster kernels.** `lib/cpuops` `ByPriority`
  candidates on the `lib/pagezero` template, aarch64 NEON first. Gated on
  the P3b axis correction below; may not land before B–C.
- **G — user-space FP/SSE enablement (kernel work).** Gated on a User
  decision; carries defect D37 below.

**Escalated for decision (§15.7):**

1. Amend `plans/FIX-HARDWARE-FEATURES.md` P3b to move the `lib/raster`
   families from the blocked `ByBenchmark` axis to the unblocked
   `ByPriority` capability axis — a packed-SIMD premultiplied `over` is
   unconditionally better and bit-identical, so it is a capability
   decision, exactly as P3a corrected page-zero. `lib/rt` already delivers
   the folded `CpuFeatureSet` to userland, so no kernel mechanism is
   needed. Blocks F.
2. Half-resolution blur changes output; approve or refuse explicitly (D.5).
   Blocks nothing, and D landed without it.
3. Whether/when to do the x86_64 user-space FPU/SSE kernel work (G).
4. **`plans/OPEN-DEFECTS.md` D37 — riscv64 appears to save no
   floating-point state** (no `fsd`/`fld` in `trap.s`/`context.s`, no
   `mstatus.FS` handling, on a hard-float target whose userland uses
   `f64`). Noticed by reading (§2.18), unconfirmed; confirm and fix
   independently of any GUI work.
