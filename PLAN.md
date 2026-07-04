# PLAN.md — RustOS Build Plan

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
  `docs-check`, `abi-check`, `c-header`, `deps-check`, `cfg-check`, `coverage`, `ci`,
  `image`.
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
  `deps-check`, `cfg-check`, `coverage`, `ci`, `image`.
- CI: `.github/workflows/ci.yml` runs `cargo xtask ci` per push/PR;
  `soak.yml` runs nightly soaks on a self-hosted Linux runner. `tools/ci/`
  holds thin `cargo xtask` wrappers (`ci-run.sh`, `soak.sh`) + scheduler
  samples; no pipeline logic in the scripts (§15).
- `ci` runs each test once (`--once` fuzz/proptest gates); the soak budget
  lives only in the time-limited GitHub soaks. Seed selection/logging/budget
  is the shared `tests/fuzzseed` (`rustos_fuzzseed`) seam: fresh seed per run,
  pinnable via `RUSTOS_{FUZZ,PROPTEST,FSSOAK}_SEED` for replay.
- `LICENSE` is GPL-2.0-or-later with the `RustOS-syscall-note` ABI exception.

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
- `lib/util` is intentionally empty (§2.3 — no item meets the ≥ 2-use rule).
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
      RwLock, MCS queue lock, SeqLock, epoch reclamation, `Once`/`OnceCell`;
      loom + proptest tests; decision tree in `docs/src/architecture/sync.md`.
- [x] 2.2 — `kernel/mem`: buddy/bitmap `FrameAllocator` over a typed
      `BootMemoryMap`, per-process `AddressSpace<P: PageTable>` (Arch HAL
      `mmu::AddressSpace + tlb::TlbShootdown` alias), guard-page kernel
      `Slab`, `alloc_sensitive`/`free_sensitive` zero-on-free, and
      `Result<_, AllocError>` everywhere (no panic on OOM).
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
Stage 3a). The boot QEMU test boots the production `rustos-kernel` pipeline
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
      `TlbShootdown` local-invalidation slice, `PageTableFrames` frame source;
      `kernel/mem` `AddressSpace<P: PageTable>` rides the HAL; wasm32 n/a).
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
      tree at runtime via `rustos_fdt::Fdt`.

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
      and the production `kernel/rustos-kernel` bin booting `kernel_main` to
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
(`kernel/rustos-kernel` bin with fail-closed dispatch callback).

**Status: complete.** Wired the production syscall path end-to-end (sub-items
f1–f7):
- f1 — per-CPU current-task slot in `kernel/sched` (`current_task`,
  `yield_current`).
- f2 — `TaskId → &TaskCapabilities` CapTable registry in `kernel/sec`.
- f3 — production `SyscallHandlers` impl in `kernel/core` + `monotonic_ns`.
- f4 — `DispatchCallbackSlot` + `Phase::Syscall` registration hook in
  `kernel/core`.
- f5 — `production_dispatch` swap + `DISPATCH_SLOT` install in
  `kernel/rustos-kernel`.
- f6 — `rustos-test-syscall-dispatch-qemu` QEMU test driving
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
  - `drivers/input/ps2` (x86_64), `drivers/input/usb_hid` (boot-protocol
    decode landed; xHCI endpoint wiring tracked in `plans/PI.md` P10).
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
  protocol lives once in `lib/fwcfg` (`rustos-fwcfg`, §2.2 — also the aarch64
  framebuffer boot console's QEMU `virt` ramfb backing). The Pi 4 EMMC2
  SD-host driver (`drivers/storage/emmc2`, an Arasan/SDHCI-5.1 SD block
  driver: PIO data transfer, **interrupt-driven completion** — the
  command/transfer waits park on the controller's bound GIC line through a
  `CompletionWait` seam rather than busy-spinning, §17.1/§2.16) ships its
  read and write paths host-tested against a register-level mock; it has no
  QEMU vertical (QEMU models no Pi EMMC2); it is accepted on metal (reads the
  FAT boot partition + RustFS root off a real Pi 4 SD card, `plans/PI.md` P8).
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
   into `include/` as `ros_driver_register_reply_t`), the `lib/rt`
   `ipc_send` wrapper, and the `lib/rt` startup-argument accessors
   (`rustos_rt::arg` / `arg_count`, published by `_start` from the
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
   before the spawner hand-off; the C view (`ros_driver_bind_key_t`,
   `ROS_DRIVER_MANIFEST_MAX_BIND_KEYS`, `ROS_DRIVER_BIND_KEY_WIRE_LEN`)
   is regenerated. Docs: `docs/src/abi/driver_traits.md`,
   `docs/src/drivers/{host,lifecycle}.md`.
3. **`userland/system/devmgr` — done.** The matcher/autoloader crate
   (`rustos-devmgr`, `no_std`, `lib/*` deps only per §17.4):
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
   tests plus the end-to-end `rustos-drvhost --test devmgr_autoload`
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
   (`rustos_fdt::name_stem`), interior buses emitted as `Bus` parents,
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
   `kernel/rustos-kernel::usb_keyboard` module is a P10 in-kernel bring-up
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
   store is reachable. The `kernel/rustos-kernel::driver_catalog`
   (`IN_KERNEL_DRIVERS`) is the in-kernel candidate list. Its legitimate
   floor is the **storage path** — the block drivers that read the volume
   holding the store: `rustos_drv_storage_virtio_blk` (virtio device id 2,
   the QEMU `virt` / x86_64 root) and `rustos_drv_storage_emmc2`
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
   `kernel/rustos-kernel::keyboard_service`, the `kernel/core`
   `InitSpawnCtx::spawn_kernel_service`/`static_frames` seam, and
   `platform::pcie_bringup`), so a metal Pi 4 can drive the video-console
   login from a USB keyboard today; the autoload migration below is the
   steady state that retires the hand-written composition. Staged
   sub-increments (one fully-gated landing each):
   - **5a — driver-declared bind tables + class-wildcard matching — done.**
     Each chain driver crate owns its canonical bind table as a
     `pub const BIND_KEYS` (`rustos_drv_bus_pcie_brcm` → compatible
     `brcm,bcm2711-pcie`; `rustos_drv_bus_usb` → xHCI PCI class
     `0x0C0330`, vendor/device wildcard; `rustos_drv_input_usb_hid` → HID
     boot keyboard `0x030101` + mouse `0x030102`, vendor/product wildcard)
     — the single source of truth a signed manifest's bind table is
     authored from (§18.3). `HwMatchKey`'s constructors are now `const`
     (so a bind table is a `const`), and `HwMatchKey::matches` adds the
     PCI/USB class-with-optional-vendor/device wildcard the matcher
     (`rustos_devmgr`) resolves against, so a generic class driver binds
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
       `UsbDevice::enumerate_hid` now reads the configuration descriptor and
       parses its first interface descriptor (`InterfaceInfo::decode`,
       fail-closed bounded walk by each `bLength`): the discovered
       `bConfigurationValue` / `bInterfaceNumber` drive `SET_CONFIGURATION` /
       `SET_PROTOCOL(boot)` (no longer hard-coded `1` / `0`), and the captured
       24-bit interface class is held as the device's identity. The new
       `UsbDevice::describe_device(parent_id, node_id)` returns an `HwNode`
       (class `Input`) carrying one `HwMatchKey::usb` of the device's
       `vid:pid` + that captured interface class — never fabricated
       (§18.5) — fail-closed `NotFound` before enumeration; the
       `usb_hid::BIND_KEYS` class-wildcard keys resolve against it. A new
       method only — no `#[repr(C)]`/C-header drift. Host-proven (the
       `InterfaceInfo::decode` fail-closed cases, the emitted-node match, the
       pre-enumeration refusal).
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
       catalogue** (`kernel/rustos-kernel::driver_catalog`) pairs each chain
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
       `rustos-drvhost` is now an aarch64 dependency; audited at
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
       and reads the device `MagicValue`); new `rustos_rt::mmio_map`; no
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
       from discovered RAM via `anon_layout::anon_window_pages` (physical RAM
       clamped to the addressable user VA above the base, floored at 16 MiB),
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
       sentinel). New `rustos_rt::dma_alloc` + `ros_sys_dma_alloc`; C header
       regenerated.
     - **5d — userland keyboard service** hosting the continuous report
       pump, autoloaded by `devmgr` over the 5d-0 surface, feeding the
       input-focus arbiter via `key_inject`. The "drivers in userland"
       steady state.
       - **5d-1 — the rt-backed `DriverHost` (`lib/drvrt`) — done
         (host-proven).** `rustos_drvrt::RtDriverHost` is the user-space
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
         `RtGrantSyscalls` → `rustos_rt`, §2.2); adds no authority, every
         check kernel-side, fail-closed (§4/§5.4/§2.9), allocation-free
         (`MAX_GRANTS`). 18 host tests; §3 + `SUMMARY.md` + `docs/src/lib/
         drvrt.md`. No production consumer yet (that is 5d-2), so no
         metal/virt step.
       - **5d-2-i — the `resource_grants` grant-delivery syscall — done
         (host-proven).** New `abi-v1` syscall **`resource_grants`** (no. 28,
         **no capability** — a task reads only its own grants, the §16.6/§24.3
         own-process baseline; unaudited) serialises the calling task's minted
         grant set from the per-task `AddressSpaceRegistry` grant table as
         consecutive `rustos_abi::hwtree::GrantedResource` records (handle +
         `HwResource`, `WIRE_LEN` = 40 — the one wire/owning definition,
         re-exported by `lib/drvrt`, §2.2), copies them out fail-closed
         (`BufferTooSmall` rather than a partial list, §2.9; `0` for an unbound
         task, §18.4). `AddressSpaceRegistry::grants_to_le_bytes` serialises;
         `RtDriverHost::from_grants_query` is the production constructor that
         issues the syscall and builds the grant table. New
         `rustos_rt::resource_grants` + `ros_sys_resource_grants`; C header
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
         production loader is `kernel/rustos-kernel::driver_spawn_loader::
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
         the TRB/ring vocabulary, and the single-device HID `UsbDevice`
         enumeration engine — therefore moved into a new `lib/usb` (`rustos-usb`)
         crate (`lib/abi`-only, `no_std`, Tier-1-portable), the USB analogue of
         `lib/virtio` ↔ `drivers/bus/virtio` (§2.2/§6/§17.4). `drivers/bus/usb`
         keeps only the §8 `register` entry, the §18.3 `BIND_KEYS` table, and the
         PCI BAR/DMA `wiring` over `rustos_usb`; the kernel scaffold and `wiring`
         repoint to `rustos_usb::{Xhci, device::*, regs}`. The 81 USB tests split
         with the code (71 protocol in `lib/usb` + 10 driver `register`/bind/
         wiring), and the whole gate is green. Now a future keyboard-driver
         *process* (and any other host-controller/HID driver) can build on
         `lib/usb` without a driver→driver edge.
       - **5d-2-ii (b-2-ii) — generic boot-keyboard orchestration + shared
         `Delay` seam — done (host-proven).** The arch-neutral
         root→hub→downstream-HID bring-up is now one definition,
         `rustos_usb::device::UsbDevice::enumerate_boot_keyboard(delay)` in
         `lib/usb` (§2.2/§18) — enumerate the first connected root-hub port and,
         when it is a hub, power/settle/find/reset/settle and address the device
         on a second slot, discovered and fail-closed. Its timed settles use the
         microsecond `Delay` seam, hoisted from `drivers/bus/pcie_brcm` into
         `lib/abi` (`rustos_abi::Delay`) so the PCIe and USB driver crates share
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
         grants) it carves the device-shared DMA region and aperture-checks it
         before any register is touched (fail closed, §5.4), maps its granted
         xHCI register BAR, brings the controller up (`rustos_usb::Xhci::open` +
         `UsbDevice::start`, carving the shared `rustos_usb::XHCI_DMA_BYTES` —
         hoisted from `bus_usb::wiring` into `lib/usb`, §2.2), and runs the
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
           The reusable HID logic moved into a new **`lib/hid` (`rustos-hid`)**
           crate — the decoders, the console producer, and the xHCI boot-keyboard
           orchestration (`bring_up_boot_keyboard`, `derive_keyboard_resources`,
           `KeyboardResources`) — the USB analogue of `lib/usb` ↔
           `drivers/bus/usb` (§2.2 / §6 / §17.4); `drivers/input/usb_hid` shrank
           to the §8 `register` + `BIND_KEYS` identity. The new
           **`drivers/input/usb_kbd` (`rustos-drv-input-usb-kbd`)** binary is the
           user-space keyboard driver: a pure-Rust `rustos-rt` program depending
           only on `lib/*` (hid/drvrt/rt/caps/abi — so §17.4 holds and the kernel
           never links `rustos-rt`) that builds `RtDriverHost::from_grants_query`
           over its kernel-issued grants (coherency `None` — kernel-coherent DMA,
           §2.20), derives its BAR + DMA aperture from the same grants with the
           host-tested `rustos_hid::derive_keyboard_resources` over the new
           `RtDriverHost::resources()` accessor (no second `resource_grants`
           syscall, §2.16), runs `bring_up_boot_keyboard`, then loops `pump_once`
           with a `KeyInjectSink` over `key_inject` + the userland `ClockDelay`,
           yielding between polls (§2.1). Fail-closed exit codes (§2.9); every
           capability + bound re-checked kernel-side (§5.4). Host-proven
           (`rustos-hid` 45, `usb_hid` 4, `drvrt` 24); usb_kbd + the aarch64
           kernel build freestanding on all three Tier-1 targets. No
           `lib/abi`/C-header change. AGENTS.md §3 + SUMMARY.md gained `lib/hid`.
           Docs: `docs/src/lib/hid.md`, `docs/src/drivers/input.md`,
           `docs/src/lib/drvrt.md`, the crate READMEs.
         - **Signed-store candidate scan — done (host-proven).** The §18.3 /
           §18.6 store scan that turns the installed `/System/Drivers/` bundles
           into autoload candidates is `rustos_drvhost::store`
           (`scan_store(source, paths, sink) -> DriverStore`): it reads each
           enumerated bundle through the existing `ImageSource`, parses the
           `.rxe` manifest with the same `ParsedImage` splitter the load gate
           uses (no drift, §2.2), decodes the bind table fail-closed, and emits
           owned `ScannedDriver`s whose `DriverStore::candidates()` lends the
           canonical `rustos_devmatch::DriverCandidate` slice
           `DeviceManager::autoload` consumes. A match step only — no authority,
           no signature check (that stays at `Host::load` when a candidate wins
           a node, §18.6); a malformed/unreadable bundle is skipped + logged
           (events 7030 accept / 7031 skip), never fatal (§18.4/§5.4). drvhost
           gained a `lib/devmatch` dep (lib/* only, §17.4). Host-proven (8
           `store::tests`); no `lib/abi`/C-header change. Docs:
           `docs/src/drivers/host.md` ("Signed-store scan").
         - **`/System/Drivers/` store enumeration (kernel half) — done
           (host-proven).** `rustos_kernel_core::driver_store::enumerate_driver_store`
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
           their *bytes*. `rustos_kernel_core::driver_store::DriverImageReader`
           builds the shared root-backed VFS once (§2.16); `read_image` reads one
           bundle off the mounted root under the uid-0 bootstrap identity —
           path-within-`/System/Drivers/`, `MAX_DRIVER_IMAGE_LEN` (16 MiB §24.4)
           bound *before* reading, full read, **appends** to the caller buffer,
           fail-closed/`buf`-untouched on refusal (§5.4/§2.9), `DriverImageError`
           →`Errno`. Since §17.4 forbids a `kernel/core`→drvhost edge, the bin
           crate supplies the read-only `/System` file service
           `rustos_kernel::system_files::SystemFileService` (Design D D2b-1):
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
           `rustos_kernel::driver_autoload::autoload_drivers` is the single
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
           `rustos_kernel::driver_autoload::autoload_from_mounted_root(fs, …)`
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
           virt`).** `InitSpawnCtx::spawn_driver_process(spawn, rxe, caps,
           grants, args)` (default fail-closed `NotImplemented`, §2.9) builds
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
             `drivers/input/virtio_kbd` (`rustos-drv-input-virtio-kbd`) is a
             freestanding `rustos-rt` program (lib/* only:
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
             `root_storage::observe_virtio_mmio_input_devices` probes each
             `virtio,mmio` slot for virtio-input (id 18) and emits a discovered
             `HwDeviceClass::Input` node carrying its register window
             (`HwResource::mmio(base, len)`, the extent from the new
             `VirtioMmioBus::slot_window`, §18.1) — the node the autoload spawn
             mints the user-space driver's window grant from (§18.3). Wired
             into `aarch64::boot` beside the block probe, host-tested,
             metal-neutral (no-op on the Pi tree, §2.17);
           - the **`-M virt` autoload vertical — done.**
             `tests/integration/autoload_input_qemu_aarch64` boots the
             production pipeline on `virt` with the
             `rustos-test-autoload-root-image` whole-disk fixture (encrypted
             rustfs root carrying the kernel-signed `virtio_kbd.rxe` at
             `/System/Drivers/input/virtio_kbd/Run`) and an attached
             `virtio-keyboard-device`: unlock → enumerate → match the discovered
             virtio-input node → verify against `KERNEL_DRIVER_SIGNER_PUBKEY` →
             spawn into a user-space process → a typed keystroke reaches the
             input-focus arbiter via `key_inject` (PASS on
             `AuditEvent::InputDelivered` `EventId(4050)`). The signing/witness
             prerequisites are landed (`KERNEL_DRIVER_SIGNING_SEED`
             single-sources both the kernel build and the fixture, §2.2; the
             one-shot `InputDelivered` witness is the
             `InputFocus::note_first_delivery` latch, carrying no key
             content/timing, §20/§23.1). The blocking-wait subsystem this and
             the interactive UART login depend on is landed: the freeing
             `rustos-kalloc` allocator (§4 deterministic OOM), `KernelProcessWait`
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
             (`build_system_partition` + `build_rpi_image`), a `RustFsSystem`
             partition role in `lib/partition`, `RustFs::open_read_only` + the
             non-secret `SYSTEM_VOLUME_KEY`, and the kernel mounting `/System`
             read-only over a `lib/partition` window in
             `root_mount::autoload_system_drivers` (audited 4140/4141). The
             `encrypted_root_image`/`autoload_root_image` fixtures author the
             split;
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
             arm admits `rustos-drv-storage-emmc2` through the signed §8 gate,
             maps the matched node's sole SDHCI register window under
             `CAP_MMIO_MAP` through a minimal in-kernel MMIO-only `Emmc2Host`,
             **discovers the controller's GIC SPI from the firmware device tree
             (`emmc2_spi`) and binds/routes/arms it on the published IRQ table**,
             and feeds the opened `Block` to the shared `finish_unlock` tail
             virtio-blk also uses (§2.2). The SD command/transfer completion
             waits **park on that bound line** through an `Emmc2Completion`
             `CompletionWait` over the same re-arming `wfi` waiter the virtio
             path uses (`RearmingIrqWaiter`, §2.2), never busy-spinning a status
             register (§17.1/§2.16 — a polled driver must not monopolise the CPU
             and starve the serial drain during `/System` autoload). On a real
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
             plus `rustos_login::supervise` acting per round (P11). The
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
         - **USB autonomous half — DONE (host-proven).** `rustos_drv_bus_usb::`
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
           `rustos_drv_bus_usb::wiring::bring_up_boot_input` maps the BAR, carves
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
         - `kernel/rustos-kernel/src/aarch64/gic_irq.rs`: the `'static`
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
         gate).** `kernel/rustos-kernel::hwtree_store::HwTreeStore` (`seed` /
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
               gate).** `kernel/rustos-kernel::shared_block`: `SharedBlock<B>`
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
               `DmaPool` / `RearmingIrqWaiter` / `KernelVirtioHost`, or the EMMC2
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
             primitive RustOS needs anyway: the disk-owning never-returning
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
               (regenerated C header + `ros_sys_*` stubs in `lib/abi-sys`), a
               monotonic **generation counter** on `hwtree_store::HW_TREE`
               (bumped on seed/append) served through `HW_TREE_SOURCE` and
               threaded into the dispatch hook by `BootInfo::with_hw_tree` (all
               three Tier-1 boot paths install it), and the signed `devmgr` rxe
               binary whose reactive observe loop (`read → wait → re-read`)
               lives host-tested behind the `rustos_devmgr::HwTreeService` seam
               (`rustos_devmgr::run`). `lib/rt` gains the `hw_tree_read`/
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
                 hook in `rustos_arch_aarch64::preempt` (`set_preempt_callback` /
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
                 hook in `rustos_arch_riscv64::preempt` (`set_preempt_callback` /
                 `on_u_mode_preempt_point`) is invoked from the supervisor-timer
                 branch of `trap::rustos_riscv64_trap_handler` after the SBI
                 re-arm, gated on the saved `frame.sstatus` SPP == 0
                 (`trap::trap_came_from_user`); `rustos_kernel::riscv64::boot`'s
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
                 preempt-callback hook in `rustos_arch_x86_64::preempt`
                 (`set_preempt_callback` / `preempt_callback`, plus the pure
                 `cs_is_ring3` origin test) is invoked from the LAPIC-timer ISR
                 `rustos_arch_x86_64_timer_dispatch` — **after** the LAPIC EOI
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
                 - The Arch HAL timer surface (`rustos_arch_api::timer::Timer`)
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
                   is `rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ` (one
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
                   binds/routes/arms it on the published IRQ table, and parks the
                   kthread on it through the same re-arming `wfi` waiter the virtio
                   path uses (`Emmc2Completion` over `RearmingIrqWaiter`, §2.2).
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
                   installs the production `rustos_arch_aarch64::preempt` surface
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
                 - **Remaining (metal only):** metal re-confirmation on a real Pi 4
                   that boot stays responsive through the slow PCIe read-back *and*
                   that the debug log keeps flowing past the passphrase prompt
                   (§0.9). The polled USB-keyboard pump remains CPU-hungry (it
                   yields but never parks); making xHCI HID interrupt-driven so it
                   parks is a separate §2.16 efficiency follow-up, not a correctness
                   blocker now that the serial drain no longer depends on the loop
                   idling.
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
           ELF→`rxe` converter and signer are the shared `rustos_itest_harness`
           definitions the kernel `build.rs` uses (§2.2); the store-planting
           routine is the single `rustos_drv_fs_rustfs::plant_nested_file`.
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
           The structural defect behind all three was then fixed (the
           level-demotions papered over it): the aarch64 `SerialSink` wrote
           each line *synchronously* to the UART via `console::tx_wait`,
           blocking the calling task — single-CPU, the whole core — for the
           line's transmit time, so *any* task's logging (devmgr's flood via
           `log_emit`, a kthread heartbeat, or program console output) could
           starve the keyboard pump. **Fixed (the design `lib/log` always
           documented):** `serial.rs` buffers **all** UART output — the
           diagnostic log sink *and* `write_console_bytes` (the
           `stream_write`/`ConsoleWrite` backing) — through one ~4 KiB byte
           ring guarded by a self-contained try-lock (`RingLock`;
           `lib/sync::SpinLock` would feature-unify `alloc` onto this port's
           allocator-less QEMU test bins, §2.2 carve-out). A producer copies
           its bytes and returns; at the end of each write the ring drains
           opportunistically (whatever the FIFO accepts now, no spin). It
           block-flushes only when the ring fills (operator-directed: bound
           memory) and goes lossy only when the UART is genuinely wedged
           (`tx_wait` drops, never hangs, §2.1); the panic bridge
           `flush_serial_blocking`s buffered context out before parking. Boot
           beacons stay on the direct lock-free `putchar` path (MMU-off, must
           trace immediately).
           The ring buffers **any** serial output, not just the log, so its
           types/API are named generically — `SerialRing` (the buffer),
           `serial::pump_tx` / `serial::flush_serial_blocking`, no
           `log`-specific names (operator-directed). The `serial` module is
           host-compiled (full host stubs), so its ring + console-model logic
           is host-unit-tested.
           **Draining never blocks the CPU; the TX interrupt + `wfi`-wake do
           it in the background (the real fix).** The operator saw the stall
           persist and the whole system feel lethargic (the `Root passphrase:`
           wait too), and was right that it was "something else": an earlier
           revision drained the dispatch loop with a **blocking** per-byte
           `putchar` spin (and refused to `wfi` while a backlog remained), so
           on this cooperative, effectively single-CPU boot the CPU spent the
           burst-heavy boot busy-waiting at the UART's byte rate instead of
           doing real work — a regression that turned a ~300 ms PCIe
           inbound-window read-back into ~4.3 s and made the prompts laggy.
           Logging must never block the CPU. **Fixed:** one shared
           non-blocking drain step (`pump` = `drain_ready` + arm `TXIM`) is used
           by the producer, the transmit ISR, and the dispatch loop
           (`pump_tx`, §2.2). `poll_interrupts` and the idle `wait_for_interrupt`
           call `pump_tx` (push what the FIFO accepts now, arm the transmit
           interrupt for the rest) then `wfi` plainly — no per-byte spin, no
           backlog re-step. The backlog drains in the background through the
           console TX interrupt (`serial::service_uart_tx_irq`), which a `wfi`
           is woken by the moment the FIFO has room, so a queued backlog flows
           at the UART's real rate with the CPU asleep between refills, then
           tickless idle (§17.1) resumes. The TX line is routed+unmasked at GIC
           bring-up with device sources masked at reset (additive, §2.17); the
           ISR reads the masked status (`ConsoleModel::tx_interrupt_fired`/
           `rx_interrupt_fired`) so it drains receive bytes only when RX
           actually fired — the passphrase FIFO-poll keeps its bytes while RX
           stays masked (§5.4, fail closed). Only a **full** ring blocks, and
           only the producer (`flush_blocking`, operator-directed: bound
           memory); a wedged UART drops bytes rather than hanging (lossy, §2.1).
           The TX register policy is in `ConsoleModel` for both PL011
           (`UARTIMSC.TXIM`/`UARTMIS`/`TXIC`) and mini-UART (`IER`/`IIR`),
           host-tested. riscv64/x86_64 do not share the flow-blocked-PL011
           defect (SBI-console / COM1 transmit), so this stays genuinely
           aarch64 glue, not stranded common logic (§2.21).
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
           reactive `devmgr` loop. Wired end to end: `rustos_rt::hw_emit_node`,
           `RtDriverHost::emit_node` (over the `GrantSyscalls` seam),
           `ros_sys_hw_emit_node` C stub + regenerated header. Host-tested
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
           `XHCI_COMPATIBLE`/`XHCI_DMA_BYTES`/`XHCI_BAR_INDEX`), the
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
              `Dma(aperture_top,XHCI_DMA_BYTES)`. The BCM2711 bring-up engine
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
              `CAP_INPUT_INJECT`): binds node B (`rustos_hid::KEYBOARD_BIND_KEYS`,
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
- `drivers/filesystem/rustfs`: native FS, copy-on-write, ACL + capability
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
- Crash-consistency tests for `rustfs` journal.
- Layout-enforcement tests: the default layout exposes exactly the four
  top-level directories; a user with root write permission may `mkdir
  /etc` (the VFS reserves no error for the name); `/System` is read-only
  at runtime except for the two writable paths listed in §16.2.

**Docs**
- `docs/src/filesystem/{overview,rustfs,ext4,fat32,permissions,layout}.md`
  (the new `layout.md` mirrors `AGENTS.md` §16).

**Status: complete.**
- Arch-neutral **VFS** in `kernel/core/src/fs/` (`path`, `perm`, `mount`,
  `vfs`): absolute-path-only parsing rejecting relative/`.`/`..`/NUL/over-long
  components, the §16.1 four-entry root template; the §5.3
  permission model (mode bits + ACL + per-inode capability gate) via one
  fail-closed `Metadata::authorize` (never branches on `uid == 0`);
  longest-prefix `MountTable` with read-only `/System` (writable `Logs`/
  `Settings`).
- Filesystem drivers: `rustfs` (native COW, journaled), `ext4` (read +
  checksummed/`64bit`/`metadata_csum` validated against `mke2fs`/`e2fsck`,
  first-party crc32c/crc16), `fat32`. Each ships a first-party `format`
  (no `mkfs` shell-out, §12) and returns `NoSpace`/`Errno::NoSpace` on
  exhaustion.
- Tests: `rustfs` journal crash-consistency soak (seeded, old-or-new
  recovery), end-to-end `rustfs`-over-virtio_blk QEMU vertical (fixture
  authored by `RustFs::format` itself, §2.2), the `pjdfstest`-equivalent
  `posix_fs_suite` over the real driver + VFS, and `fs_soak` (`cargo xtask
  fssoak`) exercising every formatter over a ≥ 1 GiB `RamBlock`.

---

## Stage 5 follow-up — RustFS (native on-disk format evolution)

**Dependencies:** Stage 5 (the VFS policy layer and the frozen
`Filesystem*` traits) and `lib/crypto`.

**Goal.** Grow the native filesystem to the full RustFS design — copy-on-write,
always-encrypted, checksummed, compressed, deduplicating, SSD-aware,
recoverable — as **one** on-disk version (no `v1`/`v2` pair) behind the frozen
`Filesystem*` traits. One mandatory profile (every feature on, not tunable);
first-party codec (no external zstd, §2.12); crypto via `lib/crypto` only. Spec:
`docs/src/filesystem/rustfs-spec.md`; user docs: `docs/src/filesystem/rustfs.md`.

**Status: all stages complete — RustFS v1 is done.** The COW `rustfs` driver
replaced the old journaled one outright (self-identifying block headers,
four-slot superblock ring, transaction root + inline commit, COW inode map)
and grew through spec Stages 2–12 into: COW B-trees, keyed-MAC + mirrored
metadata, at-rest encryption, per-record integrity, mandatory compression +
dedupe, online scrub, offline check/rescue, safe TRIM/discard, plus the
fuzz/crash-replay/corruption-injection suites. Also done: always-on **sparse
files** (metadata-only holes detected pre-hash/dedupe/compress, spec §19) and
**255-byte directory names** (263-byte slot, ext4 charset rules,
case-sensitive) with online **grow** (shrink rejected, spec §13). Passes unit
tests, the 1 GiB `fssoak`, the POSIX suite, the rustfs-over-virtio_blk QEMU
vertical, and the `fuzz_mount`/`fuzz_compress` harnesses. Per-stage legend:
spec §18.

---

## Stage 5 follow-up — RustFS extended file metadata (`plans/RUSTFS-METADATA.md`)

**Dependencies:** Stage 5 follow-up (RustFS v1) and `lib/crypto`. Design brief:
`plans/RUSTFS-METADATA.md`; spec: `docs/src/filesystem/rustfs-spec.md` §21 +
`docs/src/filesystem/metadata-registry.md`.

**Goal.** Give every RustFS inode a general-purpose, namespaced extended-
attribute store and use it to preserve foreign-filesystem per-file metadata
(Acorn/RISC OS, Amiga, Atari, classic Mac) across a copy — interoperability
with foreign data, not RustOS self-compatibility (§2.13). One shared definition
in `lib/fsmeta` (grammar, `AttrSet`/`AttrEntry`, preset registry with checked
`Time64` conversions), consumed by RustFS, the foreign-FS drivers, and the
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

**Status: foundation done.** Delivered: `lib/fsmeta` (grammar + AttrSet +
preset registry + fuzz harness), the `FilesystemAttrs` ABI, and the RustFS
attribute store (encrypt/decrypt, COW read/write, free-on-remove, free-space
rebuild accounting, reflink copy) with driver tests (round-trip/remount,
case-sensitivity, unknown-namespace + oversize + block-overflow fail-closed,
encryption at rest, read-only refusal, crash-atomicity replay, no-leak,
reflink independence, acorn preset round-trip).

**Remaining (staged, not yet built):**
- `cp`/`mv`/desktop/archive preserve-metadata tooling (the §6.2/§6.3 copy
  contract and a `getattr`/`setattr`-style CLI).
- The resource-fork *named-stream* content path for values above `VALUE_MAX`.
- Per-family foreign-FS driver wiring (lands with the ADFS/Amiga/Atari/Mac
  drivers, which do not yet exist).
- Snapshot send/receive carrying the attribute set (with `plans/RUSTFS-SNAPSHOT.md`).

---

## Stage 6 — Userland Foundations

**Dependencies:** Stages 2–5 sufficient for at least one platform.

**Deliverables**
- `userland/system/init` (PID 1): service manager, dependency-ordered start,
  reaper, capability granting from manifests.
- `userland/shell/elsh`: POSIX-ish shell with job control and a small builtin set.
- `userland/session/login`: text login that authenticates against `kernel/sec`
  and spawns a shell or a graphical session. Always starts in text mode;
  offers graphical mode only when a display driver and `userland/gui/wm`
  are available.
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
  control), `userland/session/login` (text-first, graphical only when a
  display driver + wm exist).
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
  (`rustos_kernel_core::users::load_users_db`) loads the database off the
  mounted root volume through the §5.3-checked VFS delegation, audited
  and fail-closed (proven on `-M virt` by the `users_db_qemu_aarch64`
  vertical). The kernel-neutral *install* seam is wired:
  `load_users_db_source` shares that read/parse/audit path (§2.2) but
  retains the canonical `users-v1` text in a `HeldUsersDbSource` (zeroed
  on drop §4, redacted `Debug`), and `BootInfo::with_users_db` installs
  the `Box::leak`'d holder so `run_phases` threads it into the production
  `KernelDispatchHook` (default fail-closed `NULL_USERS_DB`). The
  arch-neutral unlock + mount + load composition that *produces* that
  driver is wired (`rustos_kernel::root_mount::unlock_root_and_load_users`,
  `plans/PI.md` P11 Chunk A — host-proven): given the on-FAT `root.unlock`
  descriptor, the typed passphrase, and the encrypted root block device it
  derives the volume key (PBKDF2, zeroed-on-drop), mounts the root
  (`RustFs::open`, wrong-passphrase fail-closed), and runs
  `load_users_db_source`. The FAT `root.unlock` reader that recovers the
  first of those three inputs is wired
  (`rustos_kernel::root_mount::read_root_unlock_descriptor`, `plans/PI.md`
  P11 Chunk B-1 — host-proven): it reads the fixed-length descriptor off the
  FAT boot partition through the same real FAT32 driver that authored it
  (one on-disk definition, §2.2; the shared `ROOT_UNLOCK_NAME` constant) and
  fails closed on a missing/truncated/over-long file before any read. The
  single boot-path entry that threads those two halves together is wired
  (`rustos_kernel::root_mount::mount_root_and_load_users`, `plans/PI.md` P11
  Chunk B-2 — host-proven): given the two brought-up block devices and the
  typed passphrase it reads the descriptor and, on success, runs the unlock
  composition, auditing and fail-closing a descriptor that cannot be read
  (`RootMountError::DescriptorRead`). The single-disk entry above it is wired
  (`rustos_kernel::root_mount::mount_root_disk_and_load_users` — host-proven):
  given **one** whole-disk block device it parses the partition table through
  the shared, scheme-neutral `lib/partition` layer (MBR encode + fail-closed
  MBR/GPT parse, the one on-disk definition `tools/mkimage` writes, §2.2 /
  §2.20 — works for a Pi MBR card and a UEFI x86_64 GPT disk on any arch),
  locates the FAT boot and `RustFS` root partitions by role, opens a
  bounds-checked `PartitionBlock` window onto each in sequence (one device,
  two windows via `impl Block for &mut B`), and runs the composition —
  fail-closing a malformed/forged table or a missing partition. Root-device
  *discovery* is wired: the
  root-storage bind gate (`rustos_kernel::root_storage`, audited `4135`
  `ROOT_STORAGE_AUTOLOAD`) resolves which discovered hardware-tree node
  carries the bootstrap root block device against the in-kernel floor
  catalogue through the same shared `lib/devmatch` policy `devmgr` uses
  (§18.3 / §18.6) — read-only, fail-closed (no block device → unbound; >1 →
  ambiguous), so the metal boot is unaffected. A device behind a probed bus
  is enumerated too: the bootstrap-floor virtio-MMIO enumeration
  (`root_storage::observe_virtio_mmio_block_devices`) reads each
  `virtio,mmio` slot's `DeviceID` and folds a probed `HwMatchKey::virtio(2)`
  child node into the same selection, so the QEMU `virt` boot binds its
  virtio-blk root (a no-op on the Pi, which has no `virtio,mmio` node, §2.17).
  The board storage bring-up that *supplies* the typed passphrase and brings
  the bound driver up is wired (`plans/PI.md` P11 Chunk B-2): the init seam
  admits the in-kernel root-unlock kthread
  (`rustos_kernel::unlock_service::spawn_if_present`), which brings the bound
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
  via `rustos_login::supervise`: it **waits without prompting** while the
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
  one stream backing per discovered text console (`BootInfo::with_consoles`
  — the video console and the UART are separate session contexts), the
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
  (the UART stays its own session, fail-closed to injection). The
  keyboard *producer* is now wired host-side: the shared terminal key map
  `lib/keymap` (`encode_key` — `Key`+`Modifiers`→console tty bytes,
  allocation-free, reusing the `lib/vt` escape vocabulary, §2.2) plus the
  `drivers/input/usb_hid` `console` module (the US HID-usage→`Key` table
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

**Status: done — the library (IO1–IO3) and userland adoption (IO4) are
landed.**

The ergonomic `std::io`-equivalent library lives in `lib/rt/src/io.rs` (module
`rustos_rt::io`): one fd-generic `Read`/`Write` trait pair with looping
`read_exact`/`write_all`/`write_fmt`, buffering (`BufReader` with
`read_until`/`read_line`/`lines`, `BufWriter` coalescing small writes over a
const-generic inline buffer), and the four well-known standard streams
(`Stdin`/`Stdout`/`Stderr`/`StdInfo`) plus a non-owning `Stream` over any
inherited descriptor. It is a pure layer over the existing `abi-v1`
`stream_read`/`stream_write` traps — **no** ABI surface, syscall, or capability
(`AGENTS.md` §5.4) — `no_std` + fail-closed (§2.9). The standard streams and any
file / resource-reference / tty / pipe fd a sibling plan later opens share this
one definition (§2.2, proved by a test exercising a `Stream` over a non-standard
fd through the identical trap path). `StdInfo` (fd 3) writes are best-effort
(§20.1); opening a *new* fd stays a capability-checked operation owned by
`plans/DRIVES.md` / `plans/ALIAS.md`, never invented here. Deliberately **no**
owning/close-on-drop handle yet: `abi-v1` has no generic descriptor-close trap,
so it would be a speculative interface (§2.4) — it lands with the
descriptor-producing/closing ABI. RustOS builds **no** system-wide C `stdio`
(§16.4, `plans/CCOMPAT.md`). **IO4 done:** the in-tree callers
(`userland/shell/elsh`, `userland/system/init`, `userland/apps/top`, and the
`sysinfo`/`ps`/`top` output path shared through `lib/procinfo`) write through
`rustos_rt::io::{Stdout, Stderr, Write}` and their hand-rolled short-write
loops are deleted (§2.14) — one `Write::write_all` loop in userland. The
bounded/edit-aware line readers (the REPL's `MAX_LINE` `LineReader` and
login's prompt reads, both over the shared `rustos_vt::line::LineEditor`)
are retained as a security bound (§24.4), not the unbounded
`BufReader`. See `plans/IO.md` (binding under `AGENTS.md`).

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
  - **Left**: a "start" menu button opening a menu. The menu is **not** an
    application launcher; at this stage it is largely unpopulated and holds
    only session controls (log out, lock, shut down, restart). It is built
    so launcher entries can be added later without changing its public IPC.
  - **Middle**: a task list showing currently running tasks (one entry per
    top-level window/application), with focus/activate and minimise/restore
    on click.
  - **Right**: a clock anchored to the right-hand end, with a **notification
    icon area** immediately to its left for status/notification icons.
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
- Taskbar layout tests: start-menu button + session-control entries on the
  left, running-task list in the middle, notification area and clock on the
  right; rounded-edge rendering.
- Theme-switch tests: dark ↔ light applies consistently across WM, taskbar,
  and default apps.
- Input routing tests (focus, click-to-activate, drag-and-drop).

**Docs**
- `docs/src/desktop/{wm,taskbar,apps,theming}.md`.

**Status: in progress.**

Desktop paradigm: traditional GNOME/Windows-style `userland/gui/taskbar` (the
RISC OS iconbar idea was dropped; §3/§10 updated).

Shipped (headless-testable, model + renderer over injected seams):
- `userland/gui/wm` software compositor: premultiplied-alpha blending
  (Porter–Duff `over`), `Surface`/`geometry`, anti-aliased rounded corners via
  supersampling (square opt-out — the one rounded-corner path, §2.2), damage
  tracking, window ops; fails closed on bad modes.
- Shared desktop libs (§2.2, one path each): `lib/raster` (incl. the
  `RasterCache` SVG→scale cache), `lib/theme`, `lib/geometry` (DPI/`Scale`),
  `lib/font`, `lib/cursor`, `lib/icon`, `lib/svg`, `lib/input`, `lib/procinfo`.
- `userland/gui/taskbar` (start menu + running-task list + clock/notification
  area) and `userland/gui/session` glue (theme registry, taskbar model,
  light/dark switch, `DesktopShell` event loop / `TaskBridge`).
- Two default apps (filesystem browser, terminal emulator) — model + renderer.
- `kernel/ipc::PortRegistry` named-port registry composed into `KernelState`;
  `ipc_send`/`ipc_recv` resolve endpoints against it.

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

**Remaining this stage:**
- E — per-arch live `copy_from_user` page-fault fix-up (`tests/SECURITY.md` §5)
  so a faulting user access returns an error rather than trapping.
- Publish the desktop pointer/keyboard ports under their well-known
  `PortName`s so `IpcInputChannel`'s `MessagePort` resolves to a live
  `ipc_recv`; relay the theme switch over live IPC; wire the two default apps to
  live VFS/shell channels + WM-presented windows.
- The platform-RNG `EntropySource` that seeds the reserve — **DONE**
  (`.junie/PREREQUISITES.md` P-0): the Arch-HAL `rustos_arch_api::entropy`
  slice (x86_64 `RDSEED`/`RDRAND`, aarch64 `RNDR` `Supported`; riscv64 `Zkr` /
  wasm32 host-import honest `Pending`) seeds the kernel reserve at boot via
  `KernelArch::platform_entropy`. The seed is never the hardware RNG alone
  (§22): it is XOR-mixed with two independent software sources — a CPU
  timing-jitter source and the asynchronous interrupt-arrival-timing pool
  (`lib/rng::interrupt`, fed wait-free from `IrqTable::fire` via a set-once
  observer and folded into every reseed) — so no single source is trusted
  alone. The encrypted-swap key (Stage 8) consumes the same seam.

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
  - `images/rustos-x86_64.iso` (hybrid BIOS/UEFI).
  - `images/rustos-aarch64-rpi.img` — **DONE (landed ahead of Stage 8 as
    `plans/PI.md` P9).** `rustos-mkimage` (lib + bin) authors the image
    in pure Rust via the one-step `cargo xtask image --target aarch64-rpi`
    (or `build --target aarch64-rpi`): MBR, FAT32 boot partition (pinned,
    checksummed Pi firmware inputs per `tools/mkimage/firmware.lock` —
    fetched automatically from the manifest's pinned source when not
    operator-staged, every download checksum-gated —
    generated `config.txt`, flattened `kernel8.img`), and an encrypted
    RustFS root with the §16 skeleton, both laid down by the real
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
  - `images/rustos-riscv64.img`.
  - `images/rustos-web/` static tree.

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
point in `kernel/core`), the Arch HAL migration, the `kernel/rustos-kernel`
binary no longer naming a concrete target, virtio protocol/host relocation off
the bus driver, and the heterogeneous-CPU `core_class` (Intel + AMD CPUID
paths). No new violation may be introduced.

---

## §19 Threat Model and Hardening Burn-down

**Status:** the implementable portion is complete; the remainder is
stage-blocked, not deferred by choice. §19 supersedes the loose Stage 9
deliverables (where they conflict, §19 wins) and follows the same shrink-only,
fail-closed discipline as §17; each item lands with its own tests + docs.

**Standing directive (owner):** every *independent* burn-down item
(1, 3, 4, 5, 6, 7, 8, 11, 13) is **landed and verified green**. The remaining
items (2, 9, 10) are **stage-blocked** and carry a binding
**[DO IMMEDIATELY ON UNBLOCK]** order — the session that lands the prerequisite
stage must complete the matching §19 item before other Stage work proceeds;
item 12 stays aspirational per charter §19.7/§19.8.

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
  library** (`userland/shell/log`, crate `rustos-logtool`, §14) has landed on
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

Stage-blocked **[DO IMMEDIATELY ON UNBLOCK]**:
- Item 2 — §19.4 signed log anchors + per-service `CAP_LOG_WRITE` partitioning
  (needs a private-key signing API, Stage 2; + persisted log store, Stage 5).
- Item 9 — §19.5 parser sandboxing (minimum-capability sandbox process model,
  Stage 6).
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
- RustFS stores the four §21 timestamps as true `Time64` via a separate
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
- Kernel stack arena — `kernel/rustos-kernel/src/mem_map.rs`
  (`GUARD_ARENA_BYTES`/`GUARD_ARENA_ALIGN`, single 2 MiB block, single-shot
  carve) and `kernel/rustos-kernel/src/stack_arena.rs`
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
  a `static Aarch64ArchStorage<1>` and every aarch64 vertical a right-sized
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
  set-once and fail closed before registration (§2.9), with the §17.2/§4
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
  with no assembly `.bss` reserve. Production `rustos-kernel` registers
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
- RustFS mount footprint (`drivers/filesystem/rustfs/src/lib.rs`) — **done**
  (the §24.1 fix for the Raspberry Pi 4 eMMC2 boot OOM): both in-RAM
  allocation structures that scaled with the device block count are now
  sparse. The per-transaction private-block tracker was a dense
  `vec![false; total_blocks]` and the free-space map was a dense
  `free: Vec<u64>` bitmap (`total_blocks / 64` words allocated at mount from
  the device geometry); mounting a real multi-GiB volume allocated O(volume)
  and exhausted the 64 MiB kernel heap before any file was read. Both are now
  sparse `BTreeSet<u64>`s — the free map tracks the *used* block numbers (a
  real volume is overwhelmingly free, so the set holds the few used blocks),
  and the transaction-private set the handful of blocks a transaction touches.
  Resident size now scales with the volume's contents (working set), not its
  size. Free space is a single 64-bit count, and the transient pending-discard
  queue is capped at a fixed, volume-independent `MAX_PENDING_DISCARD` (was
  `as_usize(total_blocks)`, which saturated to `usize::MAX` on a huge device
  and could grow the queue until heap exhaustion). A `SparseBlock` test double
  proves a 100 TiB volume (~26.8 billion blocks) formats, mounts, and serves
  with only its working set resident and the discard queue bounded. **Residual
  (§26.6, not a boot blocker), next free-space work:** a near-full very large
  volume still costs RAM proportional to its *used* blocks; the structural
  answer is a paged / on-disk free-space representation (extent /
  bitmap-hierarchy) with only a bounded working-set cache resident, so even a
  near-full 100 TB+ volume mounts within a fixed RAM budget (see
  `docs/src/filesystem/rustfs-spec.md` §4).
- (Explicitly **out of scope / leave fixed**: the §22 RNG reserve
  `DEFAULT_RESERVE_BYTES`/`RANDOM_RESERVE_DEFAULT_BYTES` (charter-blessed), and
  all untrusted-input/format bounds — `lib/vt` `MAX_PARAMS`/`MAX_STRING`,
  `lib/fdt` `MAX_DEPTH`, `lib/svg` caps, ext4/fat32/rustfs format constants,
  path/name/command-line/config length caps. These are §24.4 defences.)

**Deliverables**
- L1 — **DONE.** `lib/abi` resource-limit ABI (`lib/abi/src/rlimit.rs`): closed
  versioned `LimitKind` enum (`AddressSpaceBytes`/`OpenStreams`/`Processes`/
  `StackBytes`, `COUNT`/`ALL`/`from_u32`/`name`), `ResourceLimit { soft, hard }`
  (`RLIMIT_INFINITY`, `intersect` never-widen, `encode`/`decode` fail-closed),
  the `rlimit_get` (#17) / `rlimit_set` (#18) syscalls, and `CAP_RLIMIT_RAISE`
  (id 20). Dispatcher arms route both to `SyscallHandlers::rlimit_get`/`_set`
  (default fail-closed `NotImplemented` until L2). `abi-sys` stubs
  (`ros_sys_rlimit_get`/`_set`), `lib/rt` wrappers, generated `rustos_rlimit.h`,
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
  kernel stack. `rustos_kernel_core::KTHREAD_STACK_BYTES` is now release-tuned
  (32 KiB release / 64 KiB debug, both whole 4 KiB pages, §24.2). The guard
  arena is no longer a fixed 2 MiB block: `rustos_kernel::mem_map::
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
  (`kernel/rustos-kernel/src/spawn_producer.rs` and `…_x86_64.rs`) now build a
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
  `-M virt`; production `rustos-kernel` is single-CPU and registers neither.
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
  assembly `.bss` reserve is involved. Production `rustos-kernel` registers
  `PerCpuStorage<1>` + `SyscallTlsStorage<1>`; the SMP verticals register a
  right-sized `PerCpuStorage<N>` + `ApStackPool<N>` (the `scheduler_stress_qemu`
  `MAX_CPUS <= percpu::MAX_CPUS` agreement const-assert is deleted), with the
  §17.2/§4 safety invariants preserved. **No per-arch secondary-bring-up bound
  remains.**
- L4a — **DONE.** The `ulimit` shell command in the default shell
  (`userland/shell/elsh`) over the L1 ABI. A new `rustos_elsh::LimitStore`
  seam (`get`/`set`, fail-closed `NullLimitStore` default + `Shell::with_limits`
  builder) threads through `Shell`/`BuiltinContext`; the `ulimit` builtin
  (`userland/shell/elsh/src/ulimit.rs`) parses `-a`/`-H`/`-S` + a canonical
  `LimitKind` name + a decimal/`unlimited` value, reports or imposes the
  process's own limits, preserves the unchanged bound on a one-sided set, and
  fails closed on an unknown flag/resource/value or a `soft > hard` request
  (never writing the store). The real `Run` binary installs `RtLimitStore`
  over `rustos_rt::rlimit_get`/`rlimit_set`; an in-memory `MemoryLimitStore`
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
  regenerated (`ros_resource_limit_record_t`, `ROS_SYSINFO_QUERY_RESOURCE_LIMITS`,
  `rustos_rlimit.h` include); the new decoder is in the `fuzz_decode` harness
  (§19.6). **No syscall/hash change.** Docs:
  `docs/src/architecture/resource-limits.md`, `docs/src/abi/sysinfo.md`,
  `docs/src/userland/{sysinfod,utilities}.md` + the two READMEs.

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

- C1 — `lib/vt` (`rustos-vt`): the canonical ANSI/VT/xterm vocabulary
  (control bytes, colour models, SGR, `Cell`, `Op`) with an emitter and a
  streaming parser over the same tables (emit→parse identity); the parser is
  total and fail-closed (bounded params/buffers, drops malformed input). Fuzz
  harness `fuzz_vt`.
- C2 — `userland/apps/terminal` refactored onto `lib/vt` as a *consumer* (no
  private parser); xterm-256color-class `Grid` (scroll region, alt screen,
  saved cursor, OSC title) with honest `TERM`.
- C3 — `lib/termcap` (`rustos-termcap`): compiled-in `TERM`→capability database
  (no terminfo file, §16.1) with the closed versioned `TermType` set; every
  record expressed in `lib/vt` terms; `from_term` fails closed to `Dumb`.
- C4 — `lib/curses` (`rustos-curses`): client `Window`/pad draw model, a
  minimal-diff capability-aware renderer (truecolour→256→16→mono downgrade),
  and an input decoder (keys/mouse/paste) over `lib/vt`; `Screen<T: Tty>`
  I/O-injected driver. Fuzz harness `fuzz_curses_input`. Added the key/mouse/
  paste ops to `lib/vt`.
- C5 — curses completeness (wide/UTF-8 cells, colour-pair alloc, `getch`/
  timeout input; panels deferred until a consumer needs them) + the first
  consumer `userland/apps/top` (live process TUI over `lib/procinfo` +
  `lib/curses`).
- C6 — `lib/fbcon` (`rustos-fbcon`): the shared, architecture-neutral
  framebuffer text-console engine. A full ANSI/VT/xterm-256color terminal
  (`Geometry` / `TextConsole` / `DirtyBand`, palette, glyph blit over
  `lib/font`, scroll-up-at-bottom, and the **alternate screen** — `CSI ?
  1049 h`/`l`) that renders the shared `lib/vt` `Op` stream onto a 32-bit
  scan-out surface over a **retained `rustos_vt::Cell` grid**, so every arch
  port (`x86_64`/`aarch64`/`riscv64`/`wasm32`) drives its display console
  through one definition rather than re-deriving the emulation per target
  (§2.2/§2.20/§2.21). The grid lets a full-screen program (`top`, an editor)
  enter a cleared alternate screen and, on exit, have the primary screen it
  covered restored exactly — the xterm-family contract. The two cell grids
  (primary + alternate) are **borrowed** `&mut [Cell]` the caller owns, so
  the engine stays allocator-free: a freestanding boot console with no global
  allocator supplies a `static`, an allocator-having caller leaks a heap
  buffer sized to the discovered geometry (`Geometry::cell_count`, §24.1 — no
  fixed ceiling). Depends on `rustos-vt`/`rustos-font`
  `default-features = false`; host-unit-tested (incl. alt-screen restore).
  The aarch64 port's `video.rs` consumes it two-phase: pre-MMU discovery
  records the surface and clears it; post-MMU (once the heap is usable)
  `boot` leaks the grids and calls `video::attach_console`, which builds the
  console and activates the screen — the per-CPU `Aarch64ArchStorage`
  storage-ownership pattern applied to the cell grids. The `lib/vt` `alloc`
  feature is optional (default-on) so the emitter's `Vec`-returning `encode*`
  helpers stay available to allocator-having consumers while the parser path
  is taken allocator-free by `fbcon` and the console.

## SHELL prerequisites (`.junie/PREREQUISITES2.md`)

Staged prerequisites the shell (`plans/SHELL.md`) depends on so it stays a pure
interpreter reaching effects through injected seams, with no second parser or
I/O vocabulary. See `.junie/PREREQUISITES2.md` for the full P0–P6 status.

- P6 — glob/pattern matching as a shared library: **done.** `lib/glob`
  (`rustos-glob`) is the one first-party filename-glob matcher (`*`, `?`,
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
- P4 (path parser) — shared filesystem path-spelling parser as a `lib/*` crate:
  **done.** `lib/path` (`rustos-path`) is the one definition of how a RustOS
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
  with `NotFound`. **Still open under P4** (tracked, not stubbed, future
  multi-root work — not a shell blocker): the durable `id::`/`fs::` resolver
  `Root` variants and the volume forest, at which point machine aliases rebind
  to independent `id::` roots without changing the resolver contract. Remaining
  prerequisites (P5) are tracked in `.junie/PREREQUISITES2.md`.
- P5 (reference parser) — shared resource-reference parser as a `lib/*` crate:
  **done.** `lib/resref` (`rustos-resref`) is the one definition of how a RustOS
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
  the `rustos_rt::resource_open` + `File::open_resource` wrappers, the
  `ros_sys_resource_open` C stub + regenerated header, and host/proptest/fuzz
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
  `include/rustos/` header per module + the umbrella `rustos_abi.h`, all values
  read from `lib/abi`, with a completeness test pinning every `#[repr(C)]`
  type's size/align + a tree-wide drift guard.
- CC2 — `lib/abi-sys`: the export-name-pinned `ros_sys_*` stub runtime
  marshalling into the canonical register layout and issuing the real
  `syscall`/`svc`/`ecall` (the §1 asm carve-out), panic-free, no added
  authority; host tests + a QEMU trap round-trip per native target.
- CC3 — `lib/crt0`: per-arch `_start` trampoline + allocation-free
  `build_c_runtime` (lays out C `argv`/`envp`, installs the §19.2 stack canary,
  calls `main`, routes return through `ros_sys_exit`); with kernel-side
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
the RustOS `man` command, CLI short `-h`/`-?` help, and any GUI help viewer
(§16.5 amended; rationale in "Charter Amendments").

**Landed:** (1) `BundleEntry::Help` replaced `Documentation` in place (§2.13)
with every caller/fixture updated, `docs/src/abi/appinfo.md` refreshed, and
the C header regenerated (`ROS_BUNDLE_ENTRY_HELP`); (2) `lib/help`
(`rustos-help`) — the one help engine: validated `Locale`/`DocumentName`
spellings, an injected capability-scoped `HelpSource` read seam, the
deterministic exact → same-language → `default/` fallback (served locale
reported for `man`'s `stdinfo` record), a bounded fail-closed
structured-Markdown parser (fixed section model, typed `HelpError`,
fence-aware section walk), and `render_short`/`render_full` over `lib/vt`
(widths from `lib/curses`); unit-tested, fuzz-hardened (`fuzz_help` in
`cargo xtask fuzz`), documented (`docs/src/lib/help.md`); (3) **shell
command resolution** (`plans/APPS.md` §8–§9) with the §16.2 `/System/Apps/`
system app store (charter amended, rationale in "Charter Amendments"): the
store path and bundle suffix are defined once in `lib/abi`
(`SYSTEM_APP_STORE`, `BUNDLE_SUFFIX`), every OS command app is registered
as a command-named store bundle (`/System/Apps/{elsh,ps,sysinfo,top,users}.app/Run`,
`kernel/rustos-kernel/src/spawn_paths.rs`, host drift-tested against the
shared definitions), and the shell resolves a command word through the pure
candidate policy `rustos_cmdres::resolution_candidates` (hoisted into the
shared `lib/cmdres` crate, whose `bundle_candidates` view `man`'s bundle
lookup imports; explicit paths bypass
the search; `.app` names the bundle; bare words search the store then the
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
over the shared `rustos_cmdres::bundle_candidates` order (first existing
bundle; `NotFound` moves on, any other refusal final), renders through
`lib/help`, reads the now-named `LANG` locale variable (`plans/APPS.md`
§5) and `PATH` from the inherited environment, pages on a
geometry-attested console and streams otherwise, and emits the
`help.locale_fallback` `stdinfo` `context` record on a locale fallback.
It is registered as `/System/Apps/man.app/Run` (manifest: console pair +
`CAP_FS_ACCESS`), ships its own six-locale `Help/` tree (authored on disk
in the bundle, discovered by `tools/syshelp`, and planted onto the
read-only `/System` volume's `Apps/` store by `tools/mkimage` and the QEMU
image fixture — the system volume skeleton carries `Apps/` per §16.2), and
the session-ceiling vertical
types `man man` end to end. The generic argv/stderr-line helpers were
hoisted into `lib/rt` (`args`, `io::write_stderr_line`) and the `-errno`
decode into `rustos_abi::Errno::from_syscall`, each now one definition.

Every store-registered command app (`ls`, `man`, `ps`, `top`, `sysinfo`,
`users`, `elsh`) ships its six-locale `Help/` tree and honours the
`-h`/`-?` short-help convention through the one shared `lib/help` render
(`own_short_help` + the `rt`-feature `BundleHelp` own-bundle source);
per-locale switch-drift unit tests pin each tree's `OPTIONS` to its
parser, and the tools that gained filesystem reach for the help read
request `CAP_FS_ACCESS` (the man/ls precedent).

`cargo xtask help-lint` (plans/APPS.md deliverable 5) is live: the one
shared judgement `rustos_help::lint_help_trees` (`lib/help`'s host-only
`lint` feature, also run by the `tools/syshelp` aggregator tests) gates
every discovered `Help/` tree in `cargo xtask ci`/`ci-long` — spellings,
structural bounds, `default/` presence, required-locale completeness
(`fr-FR`/`de-DE`/`es-ES`/`uk-UA`/`it-IT`), no translation-only documents,
cross-locale `OPTIONS` switch-key drift, the content-policy word screen,
and per-command coverage over the `AppInfo.toml` discovery walk.

**Remaining** (staged in `plans/APPS.md` §13): `Help/` trees for future
command apps as each becomes a registered store bundle; and wider `stdinfo`
adoption in command apps
(advisory-only, §20). A cross-app *shared-library* bundle stays declined —
§16.4 refuses cross-bundle library references — so a resource-only bundle may
share **data**, not dynamically-linked code.

**Self-contained bundles — migrate off the kernel-baked spawn registry
(charter-blocking, in progress; §16.5 amended 2026-07-04, §16.2
services-are-apps amended 2026-07-04).** Every discovered bundle (command
apps *and* the `login`/`devmgr`/`sysinfod` services) now ships complete on
the read-only `/System` volume — its signed `AppInfo` + `Run` rxe beside
its `Help/` tree, at the bundle-form paths PID 1 `init` and the shell name
— but the `spawn` syscall still dispatches the kernel-baked rxe *copies*
(`kernel/rustos-kernel/src/spawn_paths.rs` + `spawn_layout.rs`'s
`include!`-ed `*_rxe.rs` / `SPAWN_PROGRAMS`) rather than loading and
verifying the on-disk bundles, so the signed verification path is still
bypassed at launch — forbidden by the amended §16.5/§16.2 (an app *is* its
bundle directory; a service is an app; app code is never baked into the
kernel/image builder). Maintainer decision 2026-07-04: no staged
compatibility — the full correct end state, in dependency order, each
increment landing complete and green:

1. **Canonical bundle content hash** — one framing definition in `lib/abi`
   (`appinfo`) of the digest over every file the `AppInfo` signature covers
   (everything in the bundle except `AppInfo` itself, path-sorted,
   length-framed), shared by the build-time composer and every
   `BundleStore::content_hash` implementation (§2.2) — **done**:
   `rustos_abi::digest_bundle_contents` + `BundleFileDigest`, host-tested.
2. **Per-bundle manifest source + discovery + signing** — each app/service
   crate authors its own `AppInfo.toml` (id, name, version, kind
   command/service, capability request); a discovery walk over the userland
   crate roots (never a per-bundle list, §2.2/§16.5) finds them; the shared
   host composer (`rustos-itest-harness::app_image`) composes and signs the
   wire `AppInfo` under `SYSTEM_APP_SIGNING_SEED` (`build_support.rs`, a
   trust domain distinct from the driver-signing seed) — **done**:
   `AppInfo.toml` in all ten program crates,
   `app_image::{discover_app_manifests, compose_signed_appinfo}` with a
   fail-closed line-based grammar, unit tests verifying a composed
   manifest against the exact `lib/crypto` verification contract (and that
   a tampered capability body breaks it), and a `rustos-kernel` drift test
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
   `Help/` (`/System/Apps/<cmd>.app/`, `/System/Services/<name>.app/`) —
   the Pi image and every `EncryptedRootDisk` vertical carry complete
   self-contained bundles, composed once per xtask process and memoised.
   The service paths are bundle-form everywhere: PID 1 `init`'s startup
   config and the kernel registry name
   `/System/Services/{login,devmgr,sysinfod}.app/Run`, spelled from the
   shared `rustos_abi::SYSTEM_SERVICE_STORE` (drift-tested).
4. **Kernel disk-backed spawn** — the bundle-verification engine is hoisted
   out of `userland/system/appmgr` into a `lib/*` crate the kernel may link
   (§17.4), `appmgr` re-exporting it; `spawn` resolves a store-bundle path
   through the mounted VFS, verifies (signature against the kernel's
   embedded app trust anchor, content hash, ABI/syscall hash), derives the
   child's capability request from the on-disk manifest, and spawns; the
   embedded command-app/service rows leave the aarch64 production boot.
5. **Per-port storage floor, then delete the registry** — x86_64 and riscv64
   gain their bootstrap-floor disk, image layout, and read-only `/System`
   mount (their staged `tools/mkimage` builders, §12), after which
   `SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s (all but PID 1 `init`),
   `spawn_paths.rs`, and `program_manifests.rs` are deleted (§2.14). Until
   that lands, the embedded registry is those ports' explicitly-justified
   §18.6 boot floor — the only reason it still exists.

---

## USB — modular USB stack + device hot-removal (`plans/USB.md`)

**Status: planned (U1–U5).** Splits the metal-proven single-process keyboard
chain into the three independent layers the §17 modularity contracts require:
a bus driver emits the controller node, a user-space host-controller driver
(HCD, `drivers/bus/usb/xhci`) owns one controller and serves a bus-agnostic
URB transport IPC, and per-device class drivers (`drivers/input/usb_kbd`, …)
bind emitted per-interface nodes and submit URBs — no bus↔class hardwiring
(§2.20, §17.4). Hot-removal is structural: the HCD watches root-hub ports and
calls `hw_remove_node`, and `devmgr` reacts by unloading the bound driver
through a new kernel driver-unload mechanism (`StoreRequest::Unload`). U1
(kernel driver-unload + devmgr unload reaction, fully host/QEMU-testable) is
the next increment; see `plans/USB.md` for the binding design and staging.

---

## DISPLAY — seat ownership: the display/console locking model (`plans/DISPLAY.md`)

**Status: planned (D1–D6).** Closes the console/graphics *ownership and
locking* gap against Linux (DRM master + `logind` seats + tty controlling
terminal), and improves on it under the charter's fail-closed, no-ambient
model. Today `display_acquire`/`display_release` are a global `AtomicBool` in
`kernel/core/src/input_focus.rs` with no owner identity — any `CAP_DISPLAY`
holder can flip the foreground (last-writer-wins), and the `CAP_DISPLAY`
rustdoc over-claims a "cannot steal focus" guarantee the code does not enforce.
The plan makes the **seat** a first-class kernel object with a tracked,
exclusive, revocable owner (a new arch-neutral `lib/seat` state machine), folds
the existing `InputFocus` arbiter into a per-seat sink, evolves `CAP_DISPLAY`
in place (owner-checked acquire/release, no `v2`), derives the framebuffer
present right from the live lease (coupling scanout to ownership), adds
per-console controlling-terminal/foreground arbitration without signal races,
and models multi-head as N independent seats. Exactly one new capability,
`CAP_SEAT_ADMIN` (the `chvt`/`logind`-equivalent switch/revoke authority),
introduced alongside its `seatmgr` holder and enforcement point (§5.2). D1
(the `lib/seat` model) is the first increment; see `plans/DISPLAY.md` for the
binding design and staging.

---

## CAPABILITY_USE — the capability lifecycle: login → session → administration (`plans/CAPABILITY_USE.md`)

**Status: CU1–CU5 done; CU6 planned.** The full capability lifecycle is
wired: the kernel computes *effective = user grant ∩ manifest request*
(`TaskCapabilities::derive`), the runtime spawn path threads the account's
`capability_grants` ceiling through `SpawnCredential` into that intersection
(inherit copies the caller's stored ceiling; a `CAP_SPAWN_AS_USER` switch
resolves the target account's — CU1), every embedded program carries a
pinned, exactly-sized manifest with the shell on the session baseline (CU2),
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
Remaining: CU6 (desktop, blocked on `plans/DISPLAY.md`) and the installer
first-user flow (with the installer work). See `plans/CAPABILITY_USE.md`
for the binding design and staging.

---

## Cache-Aware Scheduling (LLC-aware task aggregation)

**Status: planned.** A scheduler *performance* feature (§2.16): co-locate the
threads of a process that share data onto the same Last-Level-Cache (LLC)
domain so they hit a warm shared cache instead of bouncing cache lines across
LLCs. On a machine with more than one LLC the cross-LLC miss penalty is real
and measurable; upstream Linux's cache-aware load balancing (merged for
Linux 7.2) reports double-digit gains on multi-LLC parts (e.g. AMD Zen
CCX/CCD, Intel multi-tile / sub-NUMA, and clustered ARM/RISC-V server SoCs).
RustOS supports such parts, so this is worth carrying — but only as a measured,
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
  kernel.** Amended §16.5 (and the §16.2 `/System/Apps` note) after command
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

- **2026-07-03 — `/System/Apps/`: the system app store.** Amended §16.2
  (`plans/APPS.md` §8): `/System` gains an `Apps/` subdirectory holding the
  OS-provided command apps as command-named bundles; the shell resolves a
  bare command word there *before* the user's `PATH`, so a user-writable
  directory can never shadow a system command. No new capability guards the
  store — the existing file-access and app-load gates apply (§5.2
  minimalism).

- **2026-07-03 — One bundle help tree: `Documentation/` merged into `Help/`.**
  Amended §16.5 (the merge alternative of `plans/APPS.md`, maintainer-chosen):
  the bundle's `Documentation/` entry is renamed to `Help/`, the
  internationalised structured-Markdown tree (one document per command/topic,
  one directory per BCP-47 locale plus the mandatory `default/` en-US) that is
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
  pre-release backwards-compatibility code — RustOS has not shipped, so
  RustOS-native interfaces, types, and on-disk formats are evolved *in place*
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
