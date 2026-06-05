# WIRING.md — Bringing every architecture port up to x86_64 parity

This is the staged build plan for completing the RustOS architecture
ports. `kernel/arch/x86_64` is the reference port; `kernel/arch/aarch64`,
`kernel/arch/riscv64`, and `kernel/arch/wasm32` are brought up to **at
least** the same level, target by target, in manageable increments.

`AGENTS.md` is binding — read it and `PLAN.md` first. Every rule in this
file is binding too. The continuation prompt for fresh contexts is
`.junie/next-wiring-prompt.md`.

**Note:** `abi-v1` is *not* frozen, despite what `AGENTS.md` / `PLAN.md`
say — the standing task direction supersedes that language. Changing a
`lib/abi` type today is allowed; it requires regenerating the C header
(`cargo xtask c-header --write`), which the drift guard enforces.

---

## 0. Scope and decisions (binding for this plan)

1. **HAL-first, not port-first (§17.2 / §2.2).** Parity is *enforced* by
   the Arch HAL, never copy-pasted between ports. Every primitive a port
   needs that the architecture-neutral kernel consumes is expressed as a
   trait in `kernel/arch/api`, and every port implements the *same*
   trait. Where a primitive is still ad-hoc in a port today, the wiring
   work **migrates it into a HAL trait** (the move is tracked, never a
   silent duplication). Parallel implementations of one HAL trait — one
   per arch — are the deliberate shape of §17.1/§17.2 modularity, **not**
   duplication (§2.2 carve-out): they are never collapsed behind `cfg`.
2. **No target-conditional code outside the arch crates (§17.2).**
   `cfg(target_arch …)` / `cfg(target_pointer_width …)` stay inside
   `kernel/arch/<target>/`, `.cargo/`, `tools/mkimage/`, `tools/xtask/`.
   `cargo xtask cfg-check` is the gate; its grandfather list is empty and
   stays empty.
3. **Conformance is the definition of "at parity".** A port is "at
   x86_64 level" for a HAL slice only when it passes that slice's
   conformance vertical in `kernel/arch/api/tests/` (the §17.2 / §19.1
   suites) **and** the equivalent `tests/integration/*_qemu_<arch>`
   vertical x86_64 has. "Compiles" is not "done" (`AGENTS.md` §7).
4. **Honest capability profiles (§19.1 / §19.10).** A primitive a target
   genuinely cannot provide is declared `Unsupported(reason)` /
   `Pending(note)` with a justification in the port's `README.md`, never
   faked or no-op'd silently (§2.1). wasm32 paging/userentry and riscv64
   memory-tagging are the standing honest absences.
5. **Fail closed, no hacks (§2.1 / §2.9 / §5.4).** No `unwrap`/`expect`/
   `panic!` in production paths, no `unsafe` without a `// SAFETY:` block
   + a test, no retry-until-it-works bring-up, no "single-CPU first,
   parallelise later" stubs left behind (§4).
6. **Docs + tests are part of every increment (§7 / §13).** Each stage
   updates the relevant `docs/src/platform/<arch>.md` /
   `docs/src/architecture/*.md` page and `PLAN.md`, and lands its tests
   in the same change. Tests are never deferred.
7. **One increment per landing.** Land one complete, fully-gated stage,
   update `PLAN.md` + this file, refresh `.junie/next-wiring-prompt.md`,
   then start the next.

---

## 1. Baseline — where the four ports stand today

The reference port (x86_64) ships, in `kernel/arch/x86_64`: Multiboot1/2
boot + UEFI memory-map hand-off, ACPI (RSDP/MADT) discovery, 16550
serial, APIC + IO-APIC + IDT + IRQ routing + interrupt entry/exit, the
LAPIC-timer + preemption hook, INIT-SIPI-SIPI **SMP** AP bring-up,
per-CPU storage (`percpu`), the `syscall`-instruction entry, `iretq`
user entry, a context switch, Sv-equivalent paging, TSC, and Intel
hybrid (`big`/`Atom`) **core-class** discovery.

The HAL surface currently *migrated* into `kernel/arch/api` is only four
slices: `SchedulerArch` (per-CPU id, ticks, IPI, `core_class`),
`SideChannelMitigation` (§19.1), `MemoryTagging` (§19.10), and
`EnterUser`/`UserEntry` (§17.2). Everything else (context switch, MMU,
TLB shootdown, timer, interrupt entry/exit, per-CPU storage, early-boot
discovery) is still ad-hoc inside each port — the §17.2 surface
`PLAN.md` flags as "migrated here as the §17 burn-down advances".

**Parity matrix** (✓ present, ~ partial, ✗ missing, n/a not applicable):

| Capability / HAL slice            | x86_64 | aarch64 | riscv64 | wasm32 |
| --------------------------------- | :----: | :-----: | :-----: | :----: |
| Boot stub + early console         |   ✓    |    ✓    |    ✓    |   ✓    |
| `SchedulerArch` impl              |   ✓    |    ✓    |    ✓    |   ✓    |
| Paging / MMU primitives           |   ✓    |    ✓    |    ✓    |  n/a   |
| Memory isolation QEMU vertical    |   ✓    |    ✓    |    ✓    |   ~    |
| Context switch                    |   ✓    |    ✓    |    ✓    |  n/a   |
| Timer + preemption hook           |   ✓    |    ✓    |    ✓    |   ~    |
| Interrupt controller + entry/exit |   ✓    |    ✓    |    ✓    |  n/a   |
| **SMP secondary-CPU bring-up**    |   ✓    |  **✗**  |    ✓    | **✗**  |
| **IPI delivery (real cores)**     |   ✓    |  **✗**  |    ✓    | **✗**  |
| **Early-boot platform discovery** | ✓ ACPI | **✗** FDT| ✓ FDT  |  ~ JS  |
| Per-CPU storage HAL               |   ✓    |    ~    |    ~    |   ~    |
| Syscall entry                     |   ✓    |    ✓    |    ✓    |   ✓    |
| User entry (`EnterUser`)          |   ✓    |    ✓    |    ✓    | **✗**  |
| Live `kernel/sched` task switch   |   ✓    |  **✗**  |    ~    | **✗**  |
| Heterogeneous `core_class`        | ✓ hybrid| **✗**  |  n/a   |  n/a   |
| Side-channel profile (§19.1)      |   ✓    |    ~    |    ~    |   ~    |
| Memory-tagging profile (§19.10)   |   ✓    | ~ MTE pend | ✗ unsup | ✗ unsup |
| **Arch HAL conformance suite**    | **✗ (none exists yet, all ports)** |||

**QEMU vertical parity** (`tests/integration/*`):

| Vertical                | x86_64 | aarch64 | riscv64 | wasm32 |
| ----------------------- | :----: | :-----: | :-----: | :----: |
| `kernel_arch_boot`      |   ✓    |    ✓    |    ✓    |   ✓    |
| `memory_isolation`      |   ✓    |    ✓    |    ✓    |   —    |
| `timer_preempt`         |   ✓    |    ✓    |    ✓    |   —    |
| **`ipi_smp`**           | ✓(stress)|  **✗**  |    ✓    |  **✗** |
| **`sched_drive`** (live)| ✓(stress)|  **✗**  |    ✓    |  **✗** |
| `enter_user`            |   ✓    |  ~(spawn)|  ~(spawn)|  **✗** |
| `irq`                   |   ✓    |  **✗**  |    ✓    |  n/a   |
| input (`ps2`/device)    |   ✓    |  **✗**  |  **✗**  |  n/a   |
| display (`vesa`/fb)     |   ✓    |  **✗**  |    ✓    |  **✗** |
| `virtio` blk/net        | ✓(pci) |  **✗**  | ✓(mmio) |  n/a   |

**Headline gaps, ranked:**
- **aarch64:** no SMP secondary-core bring-up, no real IPI, no FDT/DTB
  platform discovery, no live-scheduler task switch, no heterogeneous
  `core_class`, and missing IRQ / input / display / virtio verticals.
- **riscv64:** closest to parity; live-scheduler task switch is wired
  (`sched_drive_qemu_riscv64`) but not fully exercised, no per-CPU
  storage HAL trait, missing input vertical.
- **wasm32:** no multi-worker SMP, no user entry (sandbox-by-design),
  cooperative tick not wired into the live scheduler, thin verticals.
- **all ports:** the §17.2 Arch HAL conformance suite
  (`kernel/arch/api/tests/`) does not exist yet, so parity is asserted
  by inspection rather than enforced.

---

## 2. HAL surface to complete (the §17.2 burn-down)

The wiring threads through one decision: finish migrating the §17.2
surface into `kernel/arch/api` as object-safe traits, each with a
conformance vertical, then implement it once per port. The traits to add
(names indicative; finalise in review, extend `kernel/arch/api/lib.rs`
docs + §17.2 of `AGENTS.md` when each lands):

- `PlatformDiscovery` — early-boot enumeration normalised into the
  `lib/abi` hardware tree (`hwtree.rs`, §18.1/§18.2). ACPI (x86_64), FDT
  (aarch64, riscv64), host-capability query (wasm32).
- `PerCpu` — per-CPU storage handle (FS/GS base, `TPIDR_EL1`, `tp`,
  worker slot).
- `IrqController` + `InterruptEntry` — masking/arming/claim/EOI and the
  interrupt prologue/epilogue. (riscv64 already exposes
  `plic::PlicController`; generalise it.)
- `Timer` — one-shot/periodic programming + the scheduler-tick callback
  (LAPIC timer / generic timer / SBI timer / `requestAnimationFrame`).
- `ContextSwitch` — the `TaskCtx` + `switch` primitive.
- `AddressSpace` / `Mmu` + `TlbShootdown` — page-table primitives wired
  into `kernel/mem`, plus cross-CPU TLB invalidation.
- `Smp` — secondary-CPU start + directed IPI (INIT-SIPI-SIPI / PSCI
  `CPU_ON` / SBI HSM `hart_start` / Web Worker spawn).

Each trait keeps its pure bit/encoding/layout math host-testable and
gates only the register/assembly operation to the freestanding target,
exactly as the existing ports do.

---

## 3. Work breakdown (stages)

Stages are ordered so each unblocks the next; within a stage the per-arch
items are independent and can land separately. Every stage's "Definition
of done" is §4 below, run over the **whole project**.

### Stage W0 — Arch HAL conformance harness (all ports) — ✅ landed

The gate everything else is measured against.

- Create `kernel/arch/api/tests/conformance.rs` (mirroring
  `kernel/sched/api/tests/conformance.rs`): a generic, arch-agnostic
  suite parameterised over a `SchedulerArch` (+ the slices added in later
  stages) asserting the §17.2 contract — `current_cpu` stable, ticks
  monotonic non-decreasing, `send_ipi` to self is a no-op-equivalent,
  `core_class` total and panic-free, plus the §19.1 side-channel vertical
  (syscall-entry barrier present, page-table-isolation invariant,
  context-switch indirect-branch barrier) and the §19.10 `memtag`
  vertical already defined in `kernel/arch/api`.
- Each port grows a `conformance` test that instantiates the suite over
  its real `*Arch` handle (host-buildable slice).
- **Deliverable:** `cargo test -p rustos-arch-api` runs the suite; each
  port's host tests run it over their handle. Document the suite in
  `docs/src/architecture/modularity.md`.

### Stage W1 — Early-boot platform discovery HAL + aarch64 FDT

- Define `PlatformDiscovery` in `kernel/arch/api`, producing
  `lib/abi::hwtree` nodes (§18.1). Migrate x86_64 `acpi` and riscv64
  `fdt` behind it (no behaviour change; tracked move, not duplication).
- **aarch64: add an FDT/DTB reader** (`kernel/arch/aarch64/src/fdt.rs`)
  at parity with riscv64's — `/memory` map + `timebase`/PPI discovery +
  PSCI method (`hvc`/`smc`) — implementing `PlatformDiscovery`. This is
  the prerequisite for aarch64 SMP (W6).
- wasm32: the host-capability query implements `PlatformDiscovery`.
- **Deliverable:** all four ports build the hardware tree through one
  HAL trait; aarch64 `fdt` host unit tests + the conformance vertical
  pass. Docs: `docs/src/platform/{aarch64,riscv64,x86_64}.md`, §18 pages.

### Stage W2 — Per-CPU storage HAL

- Define `PerCpu`; implement over x86_64 GS-base (`percpu`), aarch64
  `TPIDR_EL1`, riscv64 `tp`, wasm32 worker slot.
- **Deliverable:** the kernel reads CPU-local state through the HAL on
  every port; conformance vertical asserts round-trip + isolation.

### Stage W3 — Interrupt controller + entry/exit HAL

- Define `IrqController` + `InterruptEntry`. Migrate x86_64
  (`apic`/`idt`/`irq`/`interrupts`), aarch64 (`gic`/`exceptions`),
  riscv64 (`plic`/`trap`) behind them.
- **aarch64: add an `irq_qemu_aarch64` vertical** matching
  `irq_qemu_x86_64` (a device IRQ routed through the GIC reaches a Rust
  handler).
- **Deliverable:** uniform IRQ surface; the new aarch64 IRQ vertical is
  QEMU-green. Docs: `docs/src/security/irq.md`, platform pages.

### Stage W4 — Timer-programming HAL

- Define `Timer`; migrate the LAPIC-timer, generic-timer, SBI-timer, and
  `requestAnimationFrame` tick sources behind it, each driving the same
  scheduler-tick callback.
- **Deliverable:** `timer_preempt_qemu_*` verticals still green on
  aarch64 + riscv64 through the HAL; conformance asserts the callback
  fires.

### Stage W5 — Context switch + MMU/paging + TLB shootdown HAL

- Define `ContextSwitch`, `AddressSpace`/`Mmu`, `TlbShootdown`; migrate
  each port's `context` + `paging` behind them and wire `AddressSpace`
  into `kernel/mem`. Add cross-CPU TLB shootdown (depends on W6 IPI on
  aarch64).
- **Deliverable:** `memory_isolation_qemu_*` verticals green through the
  HAL on every applicable port; `kernel/mem` names only the HAL trait.

### Stage W6 — aarch64 SMP secondary-core bring-up + real IPI ⭐

The single largest aarch64 gap.

- Implement `Smp` for aarch64: secondary-core start via **PSCI `CPU_ON`**
  (`hvc`/`smc`, method discovered by W1's FDT reader) with a spin-table
  fallback, a secondary-entry trampoline (`smp.s`) seeding each core's
  stack + `TPIDR_EL1`, and an `MPIDR_EL1`→dense-`CpuId` reverse map.
- Real directed IPI via **GICv2 SGI** (replacing today's single-CPU
  self-target best-effort `send_ipi`), acknowledged in the IRQ prologue
  and driving the scheduler preemption callback.
- **Add `ipi_smp_qemu_aarch64`** matching `ipi_smp_qemu_riscv64`: boot
  core starts a second core and delivers it a directed IPI (`-smp 2`).
- **Deliverable:** aarch64 runs ≥ 2 emulated cores; the new SMP vertical
  is QEMU-green; conformance SMP-stress vertical (≥ 4 cores) passes.
  Docs: `docs/src/platform/aarch64.md`, scheduler/SMP pages.

### Stage W7 — Live `kernel/sched` task switch per arch

- Wire the HAL `Timer` + `ContextSwitch` + `Smp` into the **live**
  `kernel/sched` scheduler on aarch64 (riscv64 has `sched_drive`; confirm
  + extend it; x86_64 via `scheduler_stress`).
- **Add `sched_drive_qemu_aarch64`** matching the riscv64 vertical: a
  real `Scheduler` drives `on_timer_tick` and a real task switch runs.
- **Deliverable:** every bare-metal port drives the real scheduler under
  QEMU; the new aarch64 vertical is green.

### Stage W8 — wasm32 multi-worker SMP + live cooperative scheduler

- Spawn real Web Workers, route `MessageChannel` IPIs between live
  instances (implementing `Smp` for wasm32), and wire the
  `requestAnimationFrame` tick into the live `kernel/sched` scheduler.
- Strengthen the isolation vertical into a real per-worker
  linear-memory isolation check.
- **Deliverable:** wasm32 runs multi-worker; `docs/src/platform/wasm32.md`
  updated; the browser harness exercises ≥ 2 workers.

### Stage W9 — Side-channel + memory-tagging completeness (§19.1 / §19.10)

- Fill each port's `SideChannelMitigation` profile honestly: aarch64
  CSDB/SB + KPTI-equivalent + context-switch buffer flush; riscv64
  `fence.i`/`sfence.vma` sequencing; x86_64 IBRS/STIBP/SSBD (confirm).
  No-op only where silicon is provably safe, justified in `README.md`.
- aarch64 `MemoryTagging`: progress MTE from `Pending` toward enabled as
  the Stage 6 page-table work allows (the software slab tag-check in
  `kernel/mem` already hardens UAF on every target — keep it the floor).
- **Deliverable:** the §19.1 conformance vertical passes on every port;
  profiles are honest. Docs: `docs/src/security/*`, platform pages.

### Stage W10 — Heterogeneous `core_class` discovery (aarch64)

- Override `SchedulerArch::core_class` on aarch64 with `big.LITTLE`
  classification discovered from `MPIDR_EL1` affinity / FDT
  `cpu-map`+`capacity-dmips-mhz` (x86_64 hybrid already done; riscv64
  homogeneous default stands).
- **Deliverable:** aarch64 reports asymmetric cores where present;
  conformance asserts totality + the homogeneous default elsewhere.

### Stage W11 — QEMU vertical parity sweep (drivers on aarch64)

- Close the remaining driver-facing verticals so aarch64 matches
  riscv64/x86_64: `virtio_blk_mmio_aarch64`, `virtio_net_mmio_aarch64`,
  a display vertical (framebuffer), and an input vertical. (These lean on
  the device-detection / driver-autoload stages already in `PLAN.md`
  §18 / Stage 4.)
- **Deliverable:** the QEMU matrix in §1 is filled for every arch where
  the device is emulable; each new vertical is QEMU-green and enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`.

---

## 4. Definition of done (per stage and overall) — `AGENTS.md` §7

Run over the **whole project** (never `-p`), and quote the output:

```
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all && cargo fmt --all --check
cargo xtask ci            # clippy -D warnings, deps-check, cfg-check, test matrix,
                          # docs-check, deny, c-header drift, proptest/fuzz --quick,
                          # model-check, spec-review, abi-check
cargo xtask fuzz --secs 5
tools/ci/soak.sh both --secs 10
```

The QEMU verticals are **not** in the host-only `cargo xtask ci` gate;
run the enrolled matrix separately (it is the real proof of parity):

```
cargo xtask test --qemu     # runs the whole enrolled QEMU matrix
```

A single QEMU bin can be iterated directly:

```
cargo build -p <pkg> --target <triple>
cargo run -q -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/<triple>/debug/<bin> --arch <arch> --cpus 2 --timeout-secs 60
```

A stage is done only when: its new HAL trait (if any) lives in
`kernel/arch/api` with rustdoc + a conformance vertical; every port
implements it (or declares an honest `Unsupported`/`Pending` with a
`README.md` justification); `cfg-check` / `deps-check` stay clean with
empty grandfather lists; the new/updated QEMU vertical is QEMU-green and
enrolled; docs (`docs/src/platform/<arch>.md`, the relevant
`docs/src/architecture/*` page) and `PLAN.md` + this file are updated in
the same change. Any failure found — new or pre-existing — is fixed or
reverted before the stage is done (§2.5 / §7).

---

## 5. Charter cross-references

- §1, §17.2 — Tier-1 targets; the closed Arch HAL trait set; adding a
  primitive requires a `PLAN.md` entry + an `AGENTS.md` §17.2 update.
- §17.1 / §17.4 — sibling impls are the modularity shape, not duplication
  (§2.2 carve-out); the one-way layering graph.
- §17.5 — `cargo xtask {deps-check,cfg-check}` enforcement; headless
  build is Tier-1.
- §18 — hardware tree + driver autoload the discovery HAL feeds.
- §19.1 / §19.10 — side-channel + memory-tagging conformance verticals.
- §4 — SMP from day one; no ambient authority; deterministic OOM.
- §7 / §13 — whole-project test matrix; docs land with code.
