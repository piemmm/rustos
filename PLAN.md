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
- Toolchain pinned to `nightly-2026-05-27` (rustc 1.98.0-nightly) with
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
- [x] 2.8 — Stage-2 QEMU integration: `tools/qemu` runner (ISO build, OVMF
      discovery, `isa-debug-exit`, strict wall-clock budget, no retries) +
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
  - `drivers/input/ps2` (x86_64), `drivers/input/usb_hid` (cross-arch later).
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
  `input/ps2` (x86_64), `bus/pci` (x86_64), `bus/mmio`, `bus/virtio`
  (+ in-kernel `KernelVirtioHost` with the owned-`DmaSlab` DMA shape),
  `storage/virtio_blk`, `network/virtio_net`. Each emulable driver has a
  `load → use → unload → reload` QEMU vertical; the shared `fw_cfg`/ramfb DMA
  protocol lives once in `rustos-itest-fwcfg` (§2.2).
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
`EntryResolver` is deleted). The production spawner — spawn the verified
`.rxe` payload from `/System/Drivers/` into its own address space via
`kernel/mem::build_process_image` and complete `register()` over IPC — is
the remaining half of the first Stage 4.HW increment; today's deployments
and QEMU verticals register in-process through the seam.

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
  `lib/log` with a stable event ID.
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
1. **drvhost process spawn** — the host-side half is done: the in-image
   `EntryResolver` is deleted and `Host` hands the verified manifest +
   payload to the `DriverSpawner` seam
   (`userland/system/drvhost/src/spawner.rs`), which completes the
   registration and returns the outcome — no entry pointer crosses back
   into the host; the verification half (signature, ABI version + syscall
   hashes, `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) is unchanged. Remaining: the
   kernel-side production spawner — verified `.rxe` from
   `/System/Drivers/` → `build_process_image` → spawn (`ProcessSpawn`
   shape) → `register()` handshake over IPC (a versioned `lib/abi` reply
   record, a `lib/rt` `ipc_send` wrapper, the reply endpoint id handed to
   the child via its startup args) — proven by a `-M virt` QEMU vertical
   spawning a driver-stub program; in-process `DriverSpawner` impls remain
   only in tests/verticals until the `DriverHost` surface (DMA, MMIO) is
   reachable over IPC.
2. **Bind table** — add the match-key bind table to `DriverManifest`
   (`lib/abi/src/manifest.rs`) in place (`abi-v1` is unfrozen, §2.13) and
   regenerate the C header.
3. **`userland/system/devmgr`** — the matcher/autoloader as specced above.
4. **Generic match-key emission** — replace the hand-grown list of node
   types `kernel/arch/aarch64/src/fdt.rs` recognises with generic
   match-key emission into the hardware tree, confined to
   `kernel/arch/aarch64/` (§18.2).

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
- Enforcement of the on-disk layout defined in `AGENTS.md` §16: the VFS
  refuses to create any of the reserved legacy POSIX top-level names
  (`/etc`, `/home`, `/usr`, `/var`, `/proc`, `/sys`, `/lib`, `/lib64`,
  `/bin`, `/sbin`, `/opt`, `/root`, `/tmp`, `/dev`, `/mnt`, `/media`,
  `/run`, `/boot`), and the default root template provides only
  `/System`, `/Users`, `/Apps`, `/Storage`.

**Tests**
- POSIX FS test suite (`pjdfstest`-equivalent) run under QEMU.
- ACL + capability gate tests: a user without `CAP_AUDIT_READ` cannot read
  a file marked as such, even with mode 0644.
- Crash-consistency tests for `rustfs` journal.
- Layout-enforcement tests: attempting to `mkdir /etc` (or any other
  reserved name from `AGENTS.md` §16.1) at the root returns
  `Error::ReservedPath`; `/System` is read-only at runtime except for
  the two writable paths listed in §16.2.

**Docs**
- `docs/src/filesystem/{overview,rustfs,ext4,fat32,permissions,layout}.md`
  (the new `layout.md` mirrors `AGENTS.md` §16).

**Status: complete.**
- Arch-neutral **VFS** in `kernel/core/src/fs/` (`path`, `perm`, `mount`,
  `vfs`): absolute-path-only parsing rejecting relative/`.`/`..`/NUL/over-long
  components, the §16.1 reserved-name list + four-entry root template; the §5.3
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

## Stage 6 — Userland Foundations

**Dependencies:** Stages 2–5 sufficient for at least one platform.

**Deliverables**
- `userland/system/init` (PID 1): service manager, dependency-ordered start,
  reaper, capability granting from manifests.
- `userland/shell/shell`: POSIX-ish shell with job control and a small builtin set.
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
  manifest capability granting), `userland/shell/shell` (POSIX-ish, job
  control), `userland/session/login` (text-first, graphical only when a
  display driver + wm exist).
- Core CLI utilities each as their own `userland/apps/` crate (`ls`/`cp`/`mv`/
  `rm`/`cat`/`ps`/`mount`/`chmod`/`chown`/`useradd`/`groupadd`/`setcap`/
  `getcap`/`sysinfo`); `ps`/`mount`/`sysinfo` are sysinfo-API clients (no
  `/proc`).
- App-bundle loader (signed `AppInfo` verification, granted caps = user ∩
  manifest, fixed `.app` layout enforced, §16.5) and the dynamic-loader policy
  resolving only the bundle's `Libraries/` + `/System/Libraries/` (§16.4).

### Stage 6 follow-up — Rust I/O abstraction (`plans/IO.md`)

**Status: planned (not started).**

The §20 standard-stream floor is in place: every text program does I/O over
inherited fd 0/1/2/3 through the thin `lib/rt` wrappers
(`stdout`/`stderr`/`stdinfo`/`stdin`), never a device syscall. What is missing
is the ergonomic *library* on top of those wrappers — the RustOS equivalent of
a `std::io` surface (`Read`/`Write` traits, buffered reader/writer with line
reading, and `write!`/`writeln!`-style formatting) — so shells, tools, and
services program against an abstraction instead of re-implementing the same
short-write loop and "read until newline" logic (which would be the
duplication `AGENTS.md` §2.2 forbids). It is a pure layer over the existing
`abi-v1` stream syscalls: it adds **no** ABI surface, **no** syscall, and
**no** capability (`AGENTS.md` §5.4), exposes only the four standard streams
(never a device, §20), and is `no_std` + fail-closed (§2.9). RustOS does
**not** build a system-wide C `stdio` — the *System runtime / C ABI* class
stays minimal and a third-party C program brings its own libc in its bundle
(`AGENTS.md` §16.4, `plans/CCOMPAT.md`). Staged IO1 (traits + the four stream
handles) → IO2 (buffering) → IO3 (formatting) → IO4 (adopt across userland and
delete the hand-rolled loops, §2.14) in `plans/IO.md`, which is binding under
`AGENTS.md`.

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
- The platform-RNG `EntropySource` (§17.2) that re-seeds the reserve (shared
  with the encrypted-swap key, Stage 8).

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
    `store`/`load`, the real platform-RNG `EntropySource`, the swap-device
    backend driver, and the `CAP`-gated activation syscall — all Stage 8.
- `tools/mkimage` producing:
  - `images/rustos-x86_64.iso` (hybrid BIOS/UEFI).
  - `images/rustos-aarch64-rpi.img`.
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
- §19.4 audit log — no-alloc SHA-256 hash-chain core in `lib/log` (`chain.rs`).
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
  (`userland/shell/shell`) over the L1 ABI. A new `rustos_shell::LimitStore`
  seam (`get`/`set`, fail-closed `NullLimitStore` default + `Shell::with_limits`
  builder) threads through `Shell`/`BuiltinContext`; the `ulimit` builtin
  (`userland/shell/shell/src/ulimit.rs`) parses `-a`/`-H`/`-S` + a canonical
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
