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

The HAL surface *migrated* into `kernel/arch/api` so far:
`SchedulerArch` (per-CPU id, ticks, IPI, `core_class`),
`SideChannelMitigation` (§19.1), `MemoryTagging` (§19.10),
`EnterUser`/`UserEntry` (§17.2), `PlatformDiscovery` (W1), `PerCpu`
(W2), `IrqController` + `InterruptEntry` (W3), `Timer` (W4),
`ContextSwitch` (W5a), the MMU `AddressSpace` page-table trait
(map/translate/unmap — W5b-1/W5b-2), the per-page `TlbShootdown`
slice (W5b-2), and the `PageTableFrames` page-table frame-source slice
(allocator-backed port tables via `FrameTableSource` — W5b-3), and the
`CrossCpuTlbShootdown` cross-CPU TLB-shootdown slice (W13), and the
`SecondaryBringup` SMP secondary-CPU bring-up slice (W14). With W14
landed **every** §17.2 architecture primitive now lives behind the HAL;
the burn-down is complete.

**Parity matrix** (✓ present, ~ partial, ✗ missing, n/a not applicable):

| Capability / HAL slice            | x86_64 | aarch64 | riscv64 | wasm32 |
| --------------------------------- | :----: | :-----: | :-----: | :----: |
| Boot stub + early console         |   ✓    |    ✓    |    ✓    |   ✓    |
| `SchedulerArch` impl              |   ✓    |    ✓    |    ✓    |   ✓    |
| Paging / MMU primitives (`AddressSpace` HAL) | ✓ | ✓ | ✓ | n/a |
| Cross-CPU TLB shootdown HAL (`CrossCpuTlbShootdown`) | ✓ IPI+ack | ✓ TLBI bcast | ✓ SBI RFENCE | n/a |
| Memory isolation QEMU vertical    |   ✓    |    ✓    |    ✓    | ✓ browser |
| Context switch (`ContextSwitch`)  |   ✓    |    ✓    |    ✓    |  n/a   |
| Timer + preemption HAL (`Timer`)  |   ✓    |    ✓    |    ✓    |   ✓    |
| Interrupt controller + entry/exit |   ✓    |    ✓    |    ✓    |  n/a   |
| **SMP secondary-CPU bring-up**    |   ✓    |    ✓    |    ✓    | ✓ worker |
| **IPI delivery (real cores)**     |   ✓    |    ✓    |    ✓    | ✓ MsgChan |
| **Early-boot platform discovery** | ✓ ACPI | **✗** FDT| ✓ FDT  |  ~ JS  |
| Per-CPU storage HAL               |   ✓    |    ✓    |    ✓    |   ✓    |
| Syscall entry                     |   ✓    |    ✓    |    ✓    |   ✓    |
| User entry (`EnterUser`)          |   ✓    |    ✓    |    ✓    | **✗**  |
| Live `kernel/sched` task switch   |   ✓    |    ✓    |    ✓    | ✓ coop |
| Heterogeneous `core_class`        | ✓ hybrid| ✓ FDT  |  n/a   |  n/a   |
| Side-channel profile (§19.1)      |   ✓    |    ✓    |    ✓    |   ✓    |
| Memory-tagging profile (§19.10)   |   ✓    | ✓ MTE pend | ✓ unsup | ✓ unsup |
| **Arch HAL conformance suite**    |   ✓    |    ✓    |    ✓    |   ✓    |

**QEMU vertical parity** (`tests/integration/*`):

| Vertical                | x86_64 | aarch64 | riscv64 | wasm32 |
| ----------------------- | :----: | :-----: | :-----: | :----: |
| `kernel_arch_boot`      |   ✓    |    ✓    |    ✓    |   ✓    |
| `memory_isolation`      |   ✓    |    ✓    |    ✓    |   —    |
| `timer_preempt`         |   ✓    |    ✓    |    ✓    |   —    |
| **`ipi_smp`**           | ✓(stress)|    ✓    |    ✓    | ✓(browser) |
| **`sched_drive`** (live)| ✓(stress)|    ✓    |    ✓    | ✓(browser) |
| `enter_user`            |   ✓    |  ~(spawn)|  ~(spawn)|  **✗** |
| `irq`                   |   ✓    |    ✓    |    ✓    |  n/a   |
| input (`ps2`/device)    |   ✓    |    ✓    |    ✓    |  n/a   |
| display (`vesa`/fb)     |   ✓    |    ✓    |    ✓    | ✓(browser) |
| `virtio` blk/net        | ✓(pci) | ✓(mmio) | ✓(mmio) |  n/a   |
| **`cross_cpu_tlb_shootdown`** | ✓ | ✓ | ✓ | n/a |

**Headline gaps, ranked:**
- **aarch64:** SMP secondary-core bring-up + real IPI (W6), the
  live-scheduler task switch (W7, `sched_drive_qemu_aarch64`),
  heterogeneous `core_class` discovery (W10, FDT `capacity-dmips-mhz`),
  the virtio blk/net MMIO verticals (W11-A), and the `ramfb`/framebuffer
  display vertical (W11-B), and the virtio-input vertical (W11-B) landed;
  the aarch64 QEMU matrix is now full. FDT/DTB discovery is host-tested
  via W1/W10 and the W11-A/B verticals embed the canonical `virt` DTB and
  **parse the full ARM `virt` tree at runtime** through `rustos_fdt::Fdt`
  (slot `reg`/`interrupts`, `fw_cfg` base) after their EL1 MMU bring-up;
  the embed is centralised and trimmed in W17. The MMU-off SMP verticals
  name the conduit by design (W17 — an FDT walk faults on Device memory
  pre-MMU); production conduit discovery stays the W1 `fdt::psci_method`
  path.
- **riscv64:** closest to parity; the input vertical landed
  (`input_virtio_mmio_qemu_riscv64`, the riscv64 MMIO sibling of the
  aarch64 vertical reusing the same `virtio-input` driver and shared
  `virtio_input_keypress` tail), so the riscv64 QEMU matrix is now full.
  Remaining: the live-scheduler task switch is wired
  (`sched_drive_qemu_riscv64`) but not fully exercised.
- **wasm32:** multi-worker SMP + real `MessageChannel` IPI and the
  cooperative tick wired into the live `kernel/sched` scheduler landed
  (W8, browser vertical); the framebuffer **display** vertical landed too
  (W16, browser canvas present), so the only remaining wasm32 QEMU-matrix
  gap is `enter_user` (sandbox-by-design — no user/kernel boundary).
- **all ports:** the §17.2 Arch HAL conformance suite
  (`kernel/arch/api/src/conformance.rs`, run by each port's
  `passes_arch_hal_conformance_suite`) now exists and folds in the
  `SchedulerArch`, §19.1 side-channel, §19.10 memory-tagging, platform-
  discovery, and per-CPU verticals, so those slices are enforced rather
  than asserted by inspection. The cross-CPU TLB-shootdown slice is now a
  HAL trait too (`CrossCpuTlbShootdown`, W13), and SMP secondary-CPU
  bring-up is now the `SecondaryBringup` HAL trait (W14) — no §17.2
  primitive remains ad-hoc. Every SMP QEMU/browser vertical now starts
  its secondary through that trait, not the port-private helper (W15).

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
  into `kernel/mem` (map/translate/unmap + per-page local invalidation —
  ✅ W5b-2); allocator-backed port tables via the `PageTableFrames`
  frame-source seam (✅ W5b-3); cross-CPU TLB invalidation via the
  `CrossCpuTlbShootdown` slice (✅ W13).
- `SecondaryBringup` — secondary-CPU start (INIT-SIPI-SIPI / PSCI
  `CPU_ON` / SBI HSM `hart_start` / Web Worker spawn); the directed IPI
  is already `SchedulerArch::send_ipi`, so this slice is start-only
  (✅ W14). No §17.2 primitive remains ad-hoc.

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

### Stage W1 — Early-boot platform discovery HAL + aarch64 FDT — ✅ landed

**Landed:** `lib/abi::hwtree` (the §18.1 hardware-tree ABI: `HwDeviceClass`,
`HwMatchKey`, `HwResource` as capability-grant requests, `HwNode`; pinned
`WIRE_LEN` + generated `rustos_hwtree.h`). `PlatformDiscovery` + its
`platform::conformance` vertical live in `kernel/arch/api` and are folded
into `conformance::run_all` (now four handles). The shared FDT parser was
extracted to `lib/fdt` (one parser, §2.2) with a feature-gated DTB fixture;
riscv64's `fdt` re-exports it, aarch64 gained a `fdt` query layer (PSCI
`hvc`/`smc` + generic-timer PPI) — the W6 prerequisite. Per-port impls:
x86_64 `AcpiDiscovery`, riscv64/aarch64 `FdtDiscovery`, wasm32
`HostCapabilityDiscovery`; every port's `passes_arch_hal_conformance_suite`
drives a real discovery handle.

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

### Stage W2 — Per-CPU storage HAL — ✅ landed

**Landed:** `kernel/arch/api::percpu` defines `PerCpu`
(`read_self_base` / `unsafe write_self_base` over an opaque,
full-pointer-width per-CPU base word) with a `percpu::conformance`
vertical — a single-handle `run_all` round-trip check (folded into
`conformance::run_all`, now **five** handles) and a two-handle
`run_isolation` check. Per-port impls live in a `percpu_hal` module
(struct `PerCpuStorage`): x86_64 GS-base MSR (`IA32_GS_BASE`,
`sched-arch`-gated), aarch64 `TPIDR_EL1`, riscv64 `tp`, wasm32
worker-local slot. Every port's `passes_arch_hal_conformance_suite`
drives a real `PerCpuStorage`; each port also carries host round-trip +
isolation tests. Docs: `docs/src/platform/{x86_64,aarch64,riscv64,
wasm32}.md`, `docs/src/architecture/modularity.md`.

- Define `PerCpu`; implement over x86_64 GS-base, aarch64
  `TPIDR_EL1`, riscv64 `tp`, wasm32 worker slot.
- **Deliverable:** the kernel reads CPU-local state through the HAL on
  every port; conformance vertical asserts round-trip + isolation.

### Stage W3 — Interrupt controller + entry/exit HAL

Split into two landings to keep the host-gated trait work separate from
the larger aarch64 QEMU-device work.

#### Stage W3-A — traits + per-port migration — ✅ landed

**Landed:** `kernel/arch/api::irq` defines `IrqController` (`mask` /
`unmask`, fail-closed `IrqControlError::OutOfRange`) and `InterruptEntry`
(the `claim` → `complete` prologue/epilogue), each with a host-run
`irq::conformance` vertical (`run_controller`, `run_entry`) + accept/
reject self-tests. Driven **per-port** (not folded into the five-handle
`conformance::run_all`): the controller check needs a port-specific
valid/invalid line pair and `InterruptEntry` is only on the claim-based
ports — the `percpu::run_isolation` precedent. Per-port impls: riscv64
`PlicController` (inherent mask/unmask + PLIC claim/complete, source 0 →
`None`); aarch64 `GicController` over a new host-testable `GicMmio` seam +
`Gicv2<M>` driver (`ISENABLER`/`ICENABLER` + `SeqCst` fence, `IAR`/`EOIR`,
spurious → `None`) with the freestanding `init`/`enable_ppi`/`…`/
`send_sgi` free functions now thin wrappers over the driver (one MMIO
path, §2.2); x86_64 `IoApicController` (downstream, `alloc`-bearing)
implements `IrqController` only — vectored, no claim register, so no
`InterruptEntry` (§2.1). Each port carries a host conformance test over
its real controller on a mock MMIO. Docs:
`docs/src/architecture/modularity.md`, `docs/src/security/irq.md`,
platform pages, `AGENTS.md` §17.2, `PLAN.md`.

#### Stage W3-B — aarch64 device-IRQ QEMU vertical — ✅ landed

**Landed:** `tests/integration/irq_qemu_aarch64` is the EL1/SPI analogue
of `irq_qemu_x86_64`. It binds the PL031 RTC's GICv2 SPI (INTID 34) in a
kernel-neutral `rustos_kernel_irq::IrqTable`, routes that SPI to CPU 0
through the new `gic::route_spi` (`GICD_ITARGETSR`, SPI-only; SGIs/PPIs
skipped because their target bytes are read-only/banked), installs a
set-once device-IRQ dispatcher via the new
`exceptions::set_device_irq_dispatch` hook (`handle_irq` now forwards any
non-timer INTID to it, EOI unchanged), and forwards the line to
`IrqTable::fire` over a downstream `GicController`→`kernel_irq`
`IrqController` bridge (`GicBridge`, in the test crate — the arch port
keeps no `kernel/irq` dep, §17.2). On the RTC firing, the GIC delivers
the SPI to EL1, the dispatcher masks the line + sets the wait flag, the
main loop observes `WaitStep::Ready`, and the test asserts the GIC
enable bit re-reads masked (mask-before-wake). New host tests cover the
`GICD_ITARGETSR` arithmetic, `MIN_SPI_INTID` boundary, `route_spi`
SPI-write + SGI/PPI-skip, and the fail-closed set-once dispatch slot.
Enrolled in `tools/xtask/src/commands/qemu_tests.rs` (60 s, single CPU)
and QEMU-green. Docs: `docs/src/security/irq.md`,
`docs/src/platform/aarch64.md`.

### Stage W4 — Timer-programming HAL — ✅ landed

**Landed:** `kernel/arch/api::timer` defines `Timer`
(`set_tick_callback` / `tick_callback` / `dispatch_tick`) over the
architecture-neutral `TickFn = extern "C" fn(CpuId)`, plus a host-run
`timer::conformance` vertical (`run_all`: an installed callback fires on
dispatch with the CPU it was handed; a handle with no callback dispatches
harmlessly). Each port exposes a `timer_hal::TimerHal` handle that
forwards the callback install/read to its `preempt` static and dispatches
through the trait: riscv64 (`on_timer_interrupt`), aarch64
(`on_timer_interrupt`), and wasm32 (`on_animation_frame`) now route their
tick handler through `TimerHal::dispatch_tick`, so the invoke lives in
one place (§2.2); x86_64's vectored ISR keeps its LAPIC-ID/EOI dispatch
and `TimerHal` is its HAL-facing surface. The *hardware* arming/re-arming
stays in each port's `preempt` (§2.4). The `timer_preempt_qemu_{aarch64,
riscv64}` verticals install the callback through `TimerHal` and stay
green through the HAL. Driven per-port (not folded into
`conformance::run_all`) for the same reason as `irq` — the handle is
constructed per port and reaches a port-private callback slot. Docs:
`docs/src/architecture/modularity.md`, platform pages, `PLAN.md`.

- Define `Timer`; migrate the LAPIC-timer, generic-timer, SBI-timer, and
  `requestAnimationFrame` tick sources behind it, each driving the same
  scheduler-tick callback.
- **Deliverable:** `timer_preempt_qemu_*` verticals still green on
  aarch64 + riscv64 through the HAL; conformance asserts the callback
  fires.

### Stage W5 — Context switch + MMU/paging + TLB shootdown HAL

#### Stage W5a — Context-switch HAL — ✅ landed

**Landed:** `kernel/arch/api::context` defines the architecture-neutral
`TaskContext` save area (a single `#[repr(C)]` `u64`, layout-identical to
every port's native `TaskCtx`, §2.2), the `TaskEntry` alias, the
fail-closed `PrepareError`, and the object-safe `ContextSwitch` trait
(`prepare` seeds a never-run task's first frame; `unsafe switch` performs
the bare-metal task switch), plus a host-run `context::conformance`
vertical asserting the `prepare` contract (empty context not runnable;
null/misaligned/too-small stack rejected fail-closed; good stack yields a
runnable, in-bounds frame). Re-exported from `lib.rs` and exercised by the
api integration `tests/conformance.rs` (`DoubleContextSwitch`). Per-port
impls land in a `context_hal` module (struct `ContextSwitchHal`):
x86_64/aarch64/riscv64 each reinterpret `TaskContext` as their native
`TaskCtx` (a const-assert pins the layout equality) and forward to the
existing `context` primitive — the switch invoke in one place (§2.2) — with
the bare-metal `switch` gated to the freestanding target and the host
build `unreachable!`. Each carries a host `passes_context_switch_
conformance` test. wasm32 is an honest **n/a** (no register file/stack to
swap; each "CPU" is a separate Web Worker module instance), no
`ContextSwitchHal` (§2.1). The switch itself, like `enter_user`, is proven
only under QEMU (the scheduler-drive verticals). Docs:
`docs/src/architecture/modularity.md`,
`docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`, `PLAN.md`.

#### Stage W5b-1 — MMU/page-table HAL trait + per-port migration — ✅ landed

**Landed:** `kernel/arch/api::mmu` defines the neutral `PageFlags`
permission set (`READ`/`WRITE`/`EXEC`/`USER`/`DEVICE`, W^X-aware), the
fail-closed `MapError` (`Misaligned`/`AlreadyMapped`/`PoolExhausted`/
`InvalidFlags`), the object-safe `AddressSpace` trait (`map_page` /
`root_phys` / `unsafe activate`), and a per-port-driven `mmu::conformance`
vertical (a port-constructed space + a port-specific mappable address
pair: non-null root, misaligned rejected, good map accepted, double-map
refused) with a faithful + lenient in-test double in
`kernel/arch/api/tests/conformance.rs`. Each port implements the trait on
its existing `paging::AddressSpace` — a retained `pool` field, a neutral
`PageFlags`→native leaf translation (Sv39 R/W/X/U; aarch64 W^X-aware
EL0/kernel/device leaf attrs; x86_64 user/kernel + W^X), a read-only
`leaf_present` walk for the `AlreadyMapped` guard, and `activate`
forwarding to the gated `switch` — reusing the existing walk (one walk,
§2.2) so the inherent `map_4k*` methods (used by the spawn/c-program/
abi-sys verticals) keep their signatures. riscv64 + aarch64 run
`passes_mmu_conformance` on the host (their walk recovers tables through
the identity map); x86_64's walk reaches tables through the higher-half
window (phys ≠ virt), so it is not host-runnable and its `map_page`/
`activate` are proven by the `memory_isolation` QEMU vertical instead
(the honest asymmetry the bare-metal `switch` already has). All three
`memory_isolation_qemu_*` verticals now build their victim/attacker
spaces through `AddressSpace::map_page` + `activate` (proven *through the
HAL*). wasm32 is an honest **n/a** (no page table; sandboxed linear
memory). `rustos-arch-api` became a non-optional x86_64 dep (the
always-compiled `paging` slice names it; `sched-arch` now only gates which
HAL *modules* compile). Docs:
`docs/src/architecture/modularity.md` (MMU/page-table),
`docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`, `PLAN.md`.

#### Stage W5b-2 — `kernel/mem` on the HAL + TLB shootdown — ✅ landed

- Extended the MMU HAL trait (`kernel/arch/api::mmu::AddressSpace`) with
  `translate` (read-only walk → physical page + `PageFlags`, or `None`)
  and `unmap` (tear down a 4 KiB leaf, return its frame; `NotMapped` on
  an absent/large-page address), plus `MapError::NotMapped`. The
  `mmu::conformance` vertical now asserts the full map → translate →
  double-map-refused → unmap → translate-none → unmap-`NotMapped`
  lifecycle.
- Added the **`TlbShootdown`** HAL slice (`kernel/arch/api::tlb`): a
  per-page local invalidation (`invlpg` / `tlbi vaae1is` + barriers /
  `sfence.vma`), object-safe and infallible, with its own host
  `tlb::conformance` vertical. Each bare-metal port implements it
  (host build is a vacuous no-op; the real instruction is proven by the
  spawn / `memory_isolation` QEMU verticals).
- Folded `kernel/mem`'s per-process `AddressSpace<P>` onto
  `P: mmu::AddressSpace + TlbShootdown` (the `PageTable` bound alias),
  **removing** its local `PageTableOps` trait; the façade bridges its
  `Page`/`Frame`/`MapFlags` currency to the HAL's `u64`/`PageFlags` at the
  boundary and drives `flush_page` on every map/unmap. All consumer
  generics (`kernel/{sec,virtio,core}`, `rustos-kernel`) and the six
  `{spawn,c}_program_qemu_*` integration crates renamed onto `PageTable`;
  the per-test `*UserPageTable` adapters were deleted (the ports' real
  `paging::AddressSpace` now implements the HAL traits directly).
- **Deliverable met:** `kernel/mem` names only the HAL traits; the
  per-process map/translate/unmap/flush path is exercised on the host and
  through the `memory_isolation_qemu_*` / spawn verticals.
- **Carved out to W5b-3 (tracked):** backing the per-port page tables
  with `kernel/mem`'s frame allocator instead of the static
  `PageTablePool`. This is the genuinely separable, higher-risk Stage-3a
  piece (it changes every port's `AddressSpace::new_*` signature and
  requires a HAL frame-source seam, since §17.4 forbids
  `kernel/arch/*` depending on `kernel/mem`); it lands as its own
  fully-gated increment.

#### Stage W5b-3 — allocator-backed per-port page tables — ✅ landed

- Added the **`PageTableFrames`** HAL frame-source slice
  (`kernel/arch/api::frames`): `alloc_table` hands back a `TableFrame`
  (physical address + zeroed `'static` entry view) so a port owns neither
  the storage nor the phys/virt relationship, plus the host
  `frames::conformance` vertical (fresh frame zeroed, page-aligned,
  distinct, fails closed with `None`).
- Each port's `PageTablePool` now `impl PageTableFrames` (the
  boot/bootstrap source), and every port `AddressSpace::new_*` /
  `map_4k*` / `ensure_child` takes a `&'static dyn PageTableFrames` —
  `phys_of` moved into the pool's impl. The `&'static PageTablePool` the
  QEMU/spawn crates pass coerces to the trait object unchanged, so those
  verticals stay green with no edits.
- Added `kernel/mem`'s production **`FrameTableSource`**: it draws a
  physical frame from the `FrameAllocator`, maps it through the direct
  `PhysMap`, zeroes it, hands back a `TableFrame`, and fails closed
  (returning the frame) for a frame outside the direct map. §17.4 is kept
  — `kernel/mem` depends on `kernel/arch/api`, never the reverse.
- riscv64 + aarch64 run `passes_frames_conformance` on the host (identity
  `phys_of`); `kernel/mem` runs the suite over `FrameTableSource`;
  x86_64's higher-half pool is proven through the `memory_isolation` QEMU
  vertical (the honest asymmetry the MMU slice already carries).
- **Deliverable met:** a per-process address space's internal tables come
  from the kernel frame allocator via the seam; the `memory_isolation_qemu_*`
  and spawn verticals stay green; no `cfg(target_arch …)` leaks. Docs:
  `docs/src/architecture/{modularity,memory}.md`,
  `docs/src/platform/riscv64.md`, `PLAN.md`.
- **Carried forward to W6:** cross-CPU TLB shootdown.

### Stage W6 — aarch64 SMP secondary-core bring-up + real IPI — ✅ landed

The single largest aarch64 gap, closed by mirroring the riscv64 port-side
`smp` module (no new HAL trait — riscv64 keeps SMP port-side too; an
`Smp` HAL slice remains a future §17.2 decision for both ports).

- **`kernel/arch/aarch64::psci`** (new): the PSCI `CPU_ON` firmware call
  over the conduit (`hvc`/`smc`) the W1 `fdt` reader discovers, with a
  host-tested SMC64 function-id encoding + signed-status decode
  (`PsciRet`), the aarch64 analogue of riscv64's `sbi`.
- **`kernel/arch/aarch64::smp`** (+ `smp.s`): a set-once `extern "C"
  fn(CpuId) -> !` secondary entry, a `start_secondary` launcher
  (range-checked, fail-closed `StartCpuError`) that PSCI-starts a parked
  core at the `smp.s` trampoline (which masks IRQs, seeds the core's
  `.bss` stack slice by the dense id PSCI passes as `context_id`, and
  tail-calls the entry), and `current_cpu_index` reading `MPIDR_EL1`.
- **`Aarch64Arch`** gained the dense-`CpuId`↔`MPIDR` map (`with_cpus` /
  `mpidr_of` / `cpu_for_mpidr`); `current_cpu` now reverse-maps the
  running affinity, and `send_ipi` delivers a **real GICv2 directed SGI**
  (INTID 0) — replacing the single-CPU self-target best-effort send.
- **`preempt`** gained the IPI callback surface (`set_ipi_callback` /
  `enable_ipi` / `on_ipi_interrupt`); `exceptions::handle_irq` dispatches
  an acknowledged SGI (INTID `< MIN_SPI_INTID`) to it, using
  `smp::current_cpu_index` as the one per-CPU identity source (§2.2).
- **`ipi_smp_qemu_aarch64`** (new, enrolled, `--cpus 2`, QEMU-green):
  boot core starts core 1 via PSCI and delivers it a directed SGI; PASS
  once core 1's IRQ path runs the IPI callback with core 1's id.
- **Honest carve-outs (tracked):**
  - *Non-PSCI spin-table boot* (bare Raspberry Pi 3) is **not** built:
    the QEMU `virt` board and UEFI platforms use PSCI, so a spin-table
    branch would be untested asm. It lands with a spin-table target so a
    real vertical covers it (§2.1 / §2.5), documented in
    `docs/src/platform/aarch64.md`.
  - *The QEMU vertical names the `virt` conduit (`hvc`) directly* rather
    than parsing the tree at runtime: QEMU's ELF `-kernel` boot hands no
    DTB pointer (`x0 = 0`, unlike the Linux Image protocol), and the
    shared `lib/fdt` walk does not yet handle the full ARM `virt` tree at
    runtime. Conduit *discovery* is the host-tested W1 capability; this
    `lib/fdt`-on-ARM-virt gap is carried forward (see W7 note).
- **Carried forward (still tracked):** cross-CPU TLB shootdown (from
  W5b-2/W5b-3); the `lib/fdt` runtime parse of the full ARM `virt` tree.
- **Deliverable met:** aarch64 runs ≥ 2 emulated cores under QEMU; the
  new SMP vertical is QEMU-green and enrolled. Docs:
  `docs/src/platform/aarch64.md`, `PLAN.md`, this file.

### Stage W7 — Live `kernel/sched` task switch per arch — ✅ landed

**Landed:** the aarch64 `preempt` (generic timer + GICv2 IPI) and
`context` primitives now drive the architecture-neutral `kernel/sched`
`Scheduler`, closing the last live-scheduler gap on a bare-metal port.
No new HAL trait — the existing `Timer` / `ContextSwitch` slices plus the
W6 SMP/IPI primitives are reused.

- **`tests/integration/sched_drive_qemu_aarch64`** (new, enrolled, single
  CPU, 60 s, QEMU-green): the EL1/GICv2 analogue of
  `sched_drive_qemu_riscv64`. On the `virt` board it (1) performs a real
  bidirectional `context::switch` round-trip with interrupts disabled,
  (2) builds a real `rustos-kernel-sched-mlfq::Scheduler` over
  `Aarch64Arch`, publishes it, and installs both the `preempt`
  generic-timer callback and the GICv2 IPI (SGI) callback so each drives
  `Scheduler::on_timer_tick`, then (3) brings up the EL1 vectors + GICv2,
  arms the 100 Hz generic timer + IPI, spawns 64 tasks, sends itself a
  directed IPI, and drives the cooperative `step` loop until every task
  has run. PASS once the generic-timer IRQ has driven the live scheduler
  ≥ 20 times and the IPI SGI path has driven it at least once; any
  missing path trips a dedicated failure finisher or times out
  (`AGENTS.md` §7).
- riscv64's `sched_drive_qemu_riscv64` already wires this end to end;
  x86_64 drives the live scheduler under `scheduler_stress(_qemu)`. So
  **every bare-metal port now drives the real `kernel/sched` scheduler
  under QEMU.**
- **Carried forward (still tracked):** cross-CPU TLB shootdown (from
  W5b-2/W5b-3); the `lib/fdt` runtime parse of the full ARM `virt` tree.
- **Deliverable met.** Docs: `docs/src/platform/aarch64.md`, `PLAN.md`,
  this file.

### Stage W8 — wasm32 multi-worker SMP + live cooperative scheduler — ✅ landed

**Landed:** wasm32 now boots multi-worker, routes real `MessageChannel`
IPIs between live module instances, and drives the *live* `kernel/sched`
scheduler from both the `requestAnimationFrame` tick and the IPI — the
wasm32 analogue of the W7 bare-metal work. SMP is kept **port-side** (no
new HAL trait), mirroring the riscv64/aarch64 `smp` modules; an `Smp` HAL
slice remains a future §17.2 decision for all three.

- **`kernel/arch/wasm32::smp`** (new, +host tests): the wasm32 analogue
  of riscv64's SBI HSM / aarch64's PSCI bring-up. `start_worker(n)`
  range-checks `1..MAX_WORKERS`, fails closed (`StartWorkerError`), and
  asks the host (`rustos_host_start_worker`, new `bindings` import) to
  spawn a real Web Worker that instantiates the same module as logical
  CPU `n`; `current_worker` recovers the running context's id. The host
  spawn is wasm-gated with a counter substitute so the range/decode logic
  is unit-tested under `cargo test`.
- **Host loader (`web/rustos.js`) + `web/worker.js`** (new): the loader
  gained shared `instantiate`/`runWorker`, a main-thread `boot` that
  spawns module Web Workers on `rustos_host_start_worker`, and a
  `MessageChannel` IPI hub (`rustos_host_post_ipi` → the target's
  `rustos_arch_wasm32_on_message`; worker→worker routed via the main
  thread). A worker has no `requestAnimationFrame`, so it drives its
  cooperative tick from `setTimeout` — the kernel `request_frame` is
  unchanged.
- **`isolation::live_memory_region`** (new, wasm-gated): the per-worker
  isolation check is now tied to this instance's *real* linear-memory
  size (`memory.size` × 64 KiB); every context proves it owns a live
  in-bounds address and faults an attacker confined to a disjoint region.
- **`tests/integration/kernel_arch_boot_wasm32`** (rewritten): CPU 0
  builds a live `Scheduler<WasmArch>`, arms the RAF loop driving it
  (`TICK`/frame + `step` dispatch), spawns a Web Worker (`WORKER_OK`),
  and sends it a directed IPI; CPU 1 builds its own live scheduler and
  prints `IPI_RECV` when the cross-context IPI drives it. The puppeteer
  harness now serves `/worker.js` and PASSes on
  `BOOT_OK`+`ISOLATION_OK`+`WORKER_OK`+`IPI_RECV`+≥ 20 `TICK`.
- **Deliverable met:** `cargo xtask test --wasm` is browser-green with
  ≥ 2 live workers and a live cooperative scheduler. Docs:
  `docs/src/platform/wasm32.md`, `PLAN.md`, this file.
- **Carried forward (still tracked):** cross-CPU TLB shootdown (from
  W5b-2/W6); the `lib/fdt` runtime parse of the full ARM `virt` tree.

### Stage W9 — Side-channel + memory-tagging completeness (§19.1 / §19.10) — ✅ landed

**Landed:** the §19.1 `SideChannelMitigation` and §19.10 `MemoryTagging`
HAL trait sets live in `kernel/arch/api` (`sidechannel.rs` / `memtag.rs`),
each with a portable conformance vertical, and **all four ports** carry an
honest, host-tested profile that is folded into the port's
`passes_arch_hal_conformance_suite` (so the §19.1 / §19.10 verticals run
on every port, `kernel/arch/api/src/conformance.rs::run_all`). Every
non-applied slot carries a non-empty justification (`validate`) and the
stricter `is_release_ready` gate rejects any `Pending`.

- **Side-channel profiles (`SideChannelMitigation`).** x86_64 applies
  `lfence` (syscall entry/exit) + `verw` (MDS buffer clear) and tracks
  KPTI + IBPB as `Pending` (Stage 6 page tables / CPUID-feature probe —
  the IBRS/STIBP/SSBD family rides the same CPUID-gated landing). aarch64
  applies `csdb` (Spectre-v1) and declares the MDS buffer flush
  `NotVulnerable` (Intel-only), with KPTI + the MIDR-specific Spectre-v2
  sequence `Pending`. riscv64 emits a conservative memory `fence` and is
  release-ready: the in-order cores RustOS targets (QEMU `virt`, SiFive
  U54/U74) do not speculate past a fault or mispredict, so the Meltdown /
  MDS / Spectre-v2 controls are justified no-ops — `fence.i`/`sfence.vma`
  *speculation* sequencing is not needed on in-order silicon and is
  revisited only if an out-of-order RISC-V core is added. wasm32 is
  release-ready: every microarchitectural control is host-owned
  (site isolation, timer clamping, COOP/COEP) and memory is isolated per
  Web Worker. Each barrier instruction is `cfg`-gated to bare metal under
  a `// SAFETY:` block.
- **Memory-tagging profiles (`MemoryTagging`).** aarch64 implements the
  real Arm MTE `stg` store sequence (`#[target_feature(enable = "mte")]`,
  16-byte / 4-bit granule) behind a default-off `mte_enabled` gate and
  declares both slots `Pending` on the Stage 6 `FEAT_MTE` probe + `Normal
  Tagged` mapping; x86_64 / riscv64 / wasm32 declare a justified
  `Unsupported` (no per-granule tagging silicon). The architecture-neutral
  `next_free_tag` rotation is shared by the ports and by the software UAF
  floor.
- **Software UAF floor stays on everywhere.** `kernel/mem`'s slab rotates
  the slot tag on every allocation and rejects a stale-tag handle with
  `SlabError::TagMismatch`; `SoftwareTagCheck::for_tagging` only stands it
  down where a port `enforces_uaf_in_hardware()` (no Tier-1 port does
  yet), so `Slab::new` keeps it enabled on all four targets.
- **Deliverable met:** the §19.1 + §19.10 conformance verticals pass on
  every port (`cargo test -p rustos-arch-{x86_64,aarch64,riscv64,wasm32}`
  + `-p rustos-kernel-mem` green); profiles are honest. Docs:
  `docs/src/security/side_channels.md`, `docs/src/security/memory_tagging.md`,
  platform pages, `PLAN.md` §19 items 8 & 13, this file.
- **Carried forward (Stage-6-blocked, "[DO IMMEDIATELY ON UNBLOCK]"):**
  KPTI + IBPB/IBRS/STIBP/SSBD on x86_64; KPTI + the MIDR Spectre-v2
  sequence on aarch64; auto-enabling Arm MTE on `FEAT_MTE` silicon. All
  three depend on the Stage 6 user/kernel page-table boundary, not on this
  stage; the software slab tag-check remains the UAF floor until then.

### Stage W10 — Heterogeneous `core_class` discovery (aarch64) ✅ LANDED

- aarch64 now overrides `SchedulerArch::core_class` with `big.LITTLE`
  classification discovered from the device tree's per-core
  `capacity-dmips-mhz` ratings (x86_64 hybrid already done; riscv64
  homogeneous default stands).
- **Shared FDT reader (`lib/fdt`).** Added `Fdt::each_cpu`, a focused,
  allocation-free walk over `/cpus/cpu@*` that yields each node's `reg`
  (`MPIDR_EL1` affinity) and optional `capacity-dmips-mhz`, plus an
  `arm_with_cpus` `big.LITTLE` fixture. One device-tree parser, shared
  by every arch (§2.2); host-tested.
- **Pure classifier (`kernel/arch/aarch64::hetcore`).**
  `class_for_capacity` maps a core at the peak advertised rating (or with
  no rating) to performance and any core strictly below the peak to
  efficiency; homogeneous (equal / absent ratings) and a missing rating
  fail conservative to performance (§2.9). Pure and host-tested.
- **Port wiring (`Aarch64Arch`).** A caller-sized per-CPU `core_classes`
  table borrowed from `Aarch64ArchStorage<N>` (§24.1, no `MAX_CPUS`
  ceiling), `record_core_class`, `classify_from_fdt` (two device-tree
  passes — find the peak, then classify each cpu node's affinity to a
  dense `CpuId` — no fixed buffer), and the `core_class` override
  (out-of-range → performance, never a panic). The boot consumer calls
  `classify_from_fdt` once on the boot core.
- **Deliverable met:** aarch64 reports asymmetric cores where present
  (`classify_from_fdt_reports_big_little_cores`) and the homogeneous
  default everywhere else; the shared HAL conformance vertical
  (`core_class_is_total`, run by every port via `run_all`) asserts
  totality. `cargo test -p rustos-arch-aarch64 -p rustos-fdt` green.
  Docs: `docs/src/platform/aarch64.md`, `PLAN.md`, this file.

### Stage W11 — QEMU vertical parity sweep (drivers on aarch64)

#### Stage W11-A — virtio blk + net MMIO verticals (aarch64) — ✅ landed

- aarch64 now runs the virtio-blk and virtio-net `virt`-board MMIO
  verticals, the EL1/GICv2 analogue of the riscv64 ones:
  `tests/integration/virtio_blk_mmio_aarch64` (sector-0 verify +
  sector-1 write/read-back) and `tests/integration/virtio_net_mmio_aarch64`
  (ARP-resolve the SLIRP gateway + ICMP echo), both QEMU-green and
  enrolled in `tools/xtask/src/commands/qemu_tests.rs`.
- **Shared bring-up (`tests/integration/virtio_qemu_support`).** Added the
  `imp_mmio_aarch64` module behind `cfg(itest_aarch64)`: it enables FP/SIMD
  at EL1 (`CPACR_EL1.FPEN`), brings up a 2 GiB identity-mapped stage-1 MMU
  (GiB 0 Device, RAM Normal-cacheable — the precondition for the
  atomic-heavy driver/DMA/sync stack, which riscv64 gets from its boot
  pipeline), provisions the transport through the capability-gated
  `KernelMmioMapper`, walks the DTB for the device's GICv2 SPI, wires the
  EL1 device-IRQ dispatch to a `kernel/irq` `IrqTable` over a
  `GicController` bridge, and parks on a race-free `wfi` (DAIF-masked).
  The device-agnostic lifecycle and the blk/net round-trip tails are the
  *same* shared code the riscv64 / x86_64 verticals run (§2.2);
  `dtb_total_size` moved into the shared `common` module.
- **DTB hand-off.** QEMU's `-kernel <ELF>` aarch64 path treats the image
  as bare firmware and passes no DTB pointer (x0 = 0), so each vertical
  embeds the canonical `virt` DTB, dumped at build time by
  `qemu-system-aarch64 ... dumpdtb` (gated to the aarch64-none target),
  and hands those bytes to the scenario. The transport bases and SPIs in
  that blob are the stable `virt`-board layout, independent of which slot
  the backing device lands on.
- **Verified:** both verticals exit `0` under `qemu-system-aarch64 -M virt`
  (the `cargo xtask test --qemu` enrolment path).

#### Stage W11-B — display + input verticals (aarch64) — ✅ landed

- **Display (landed).** aarch64 now runs the `ramfb`/framebuffer display
  vertical, the EL1/GICv2 analogue of `framebuffer_display_qemu_riscv64`:
  `tests/integration/framebuffer_display_qemu_aarch64`
  (`rustos-test-framebuffer-display-qemu-aarch64`) programs QEMU's `ramfb`
  over `fw_cfg`, assembles the geometry as a `FramebufferConfig`, loads
  the signed framebuffer `.rxe` through `rustos_drvhost::Host`, and drives
  `load → use → unload → reload` (mapping the surface through the
  capability-gated `KernelMmioMapper` and reading the presented pixels
  back through an independent window). QEMU-green and enrolled in
  `tools/xtask/src/commands/qemu_tests.rs` (`ramfb: true`).
- **Shared, not duplicated (§2.2).** The W11-A EL1 FP-enable + 2 GiB
  identity-MMU bring-up was extracted into a public
  `bring_up_el1_identity_mmu(&dyn QemuEnv)` (and the env type made public
  as `AArch64QemuEnv`), reused by both the virtio scenario and the display
  vertical. The `fw_cfg` MMIO transport (`MmioDma`) is byte-identical on
  the riscv64 and aarch64 `virt` boards, so it was moved into the shared
  `rustos-itest-fwcfg` crate and now serves both display verticals (the
  riscv64 vertical's local copy was deleted); only the x86_64 IOport
  transport stays distinct. The display driver lifecycle is the per-arch
  sibling of the riscv64/x86_64 display scenarios (the established
  per-vertical pattern), differing only in the EL1 bring-up + embedded DTB.
- **Input (landed).** aarch64 now runs the virtio-input vertical, the
  `virt`-board analogue of the x86 PS/2 vertical, filling the `input` row
  of the §1 QEMU matrix: `tests/integration/input_virtio_mmio_qemu_aarch64`
  (`rustos-test-input-virtio-mmio-qemu-aarch64`) reuses the same
  `bring_up_el1_identity_mmu` + embedded-DTB path, builds the virtio-MMIO
  transport, arms the GICv2 SPI, loads the signed virtio-input `.rxe`, and
  drives `load → use → unload → reload`. Enrolled with `keyboard: Some(..)`.
- **New driver + shared tail (§2.2).** `drivers/input/virtio_input`
  (`rustos-drv-input-virtio-input`) implements the `Input` trait over the
  bus-agnostic `lib/virtio` transport; the device round-trip tail
  `virtio_input_keypress` lives in the shared `virtio_qemu_support` crate,
  so a riscv64 MMIO sibling is a thin new bin. The driver **pre-posts a
  pool of eventq buffers** — QEMU's virtio-input completes a buffer per
  event of a report, so a keypress's `EV_KEY` *and* its `EV_SYN` each need
  one in flight — and negotiates `VIRTIO_F_VERSION_1`.
- **Real injected key (`tools/qemu`).** "Use" is a genuine device→driver
  event, the analogue of the PS/2 `0xD2` injection: the runner attaches a
  `virtio-keyboard-device` (`Spec::with_virtio_keyboard`), drains the
  serial console on a background thread, and on the guest's readiness
  marker sends `sendkey` over a private-socket QEMU monitor, holding that
  connection open until the run ends (a readline monitor drops a command
  on early disconnect). A `--virtio-keyboard <marker> <key>` flag exposes
  the same on `rustos-qemu-run`.
- **Verified:** the display and input verticals exit `0` under
  `qemu-system-aarch64 -M virt` (the `cargo xtask test --qemu` enrolment
  path). Docs: `docs/src/platform/aarch64.md`, `docs/src/drivers/display.md`,
  `docs/src/drivers/input.md`, the framebuffer + virtio-input driver
  `README.md`s, `PLAN.md`, this file.

#### Stage W11-C — input vertical (riscv64) — ✅ landed

The last riscv64 §1 QEMU-matrix gap. riscv64 now runs the
**virtio-input** MMIO vertical — the `virt`-board sibling of the aarch64
input vertical — filling the `input` row for riscv64; the riscv64 matrix
is now full.

- **New vertical, thin by design (§2.2).**
  `tests/integration/input_virtio_mmio_qemu_riscv64`
  (`rustos-test-input-virtio-mmio-qemu-riscv64`) reuses the exact
  `imp_mmio` bring-up the riscv64 blk/net verticals run (DTB virtio-MMIO
  walk, `CAP_MMIO_MAP`-gated `MmioTransport`, PLIC source + S-mode trap
  dispatch, `KernelVirtioHost`), then loads the signed virtio-input `.rxe`
  and drives `load → use → unload → reload`. It differs from the net
  sibling only in the device id (`18`, virtio-input) and the resolver
  binding the image to `rustos_drv_input_virtio_input::register`; the
  `virtio_input_keypress` key-decode tail is the same shared
  `virtio_qemu_support` code the aarch64 vertical runs. No new driver and
  no new shared scaffolding were needed — the W11-B work left this a thin
  bin, exactly as planned. Enrolled with `keyboard: Some(..)`.
- **Real injected key (`tools/qemu`).** The runner's monitor key-injection
  path (drain serial → on readiness marker `sendkey` over a private-socket
  QEMU monitor held open until run end) is architecture-neutral; the only
  riscv64 runner change is the `virtio-keyboard-device` attach in the
  riscv64 argv builder (`tools/qemu/src/riscv64.rs`), with matching argv
  unit tests. The same `rustos-qemu-run --arch riscv64 --virtio-keyboard
  <marker> <key>` flag drives it by hand.
- **Verified:** the bin exits `0` under `qemu-system-riscv64 -M virt`
  (the `cargo xtask test --qemu` enrolment path; also reproducible via
  `rustos-qemu-run --arch riscv64 --virtio-keyboard "<marker>" a`). Docs:
  `docs/src/platform/riscv64.md`, `docs/src/drivers/input.md`, the
  virtio-input driver `README.md`, `PLAN.md`, this file.

#### Stage W12 — one device-tree parser (`lib/fdt` node API) — ✅ landed

The workspace carried **two** flattened-device-tree parsers: the shared
`lib/fdt` reader (header + path-property + memory/timebase/`each_cpu`
queries) and a second, full node-iteration parser in `lib/util/dtb`
(`Dtb`/`Node`/`Property`) that `drivers/bus/mmio` and the QEMU verticals
walked the `virt` tree through. That is the duplication `AGENTS.md` §2.2
forbids. W12 folds the two into one: `lib/fdt` now owns the generic
node-iteration API for the full ARM/riscv `virt` tree, and the duplicate
is deleted.

- **`lib/fdt` node API (the full `virt` tree).** Added `Fdt::nodes()`
  yielding `Node` handles with `is_compatible`, `property`/`properties`,
  `name`/`depth`, and `Property::{read_be_u32,read_be_u64,iter_strings}` —
  the surface every consumer needs to enumerate `virtio,mmio` / `fw_cfg` /
  `plic` nodes and read their `reg`/`interrupts`/`riscv,ndev` cells. The
  walk reuses the existing token primitives (`read_node_name`/`read_prop`/
  `string_at`, refactored into shared free functions — one implementation,
  §2.2), is allocation-free, bounds-checks every read, and fails closed:
  the iterator yields `Err(FdtError)` and stops on a malformed token, and
  out-of-range cell reads return `FdtError::OutOfBounds` (§2.9). Verified
  against the real 1 MiB QEMU `virt` DTB during development; covered by new
  host tests (virtio-mmio slot enumeration with `reg`+`interrupts`,
  `is_compatible` true/false/absent, fail-closed reads, malformed-token
  fail-closed).
- **All consumers migrated; duplicate deleted (§2.2).**
  `drivers/bus/mmio` (`enumerate`/`lib`/`tests`) and the verticals
  `virtio_qemu_support` (`imp_mmio` riscv64 PLIC + `imp_mmio_aarch64`
  GICv2 SPI), `fwcfg`, and `framebuffer_display_qemu_{aarch64,riscv64}`
  now parse through `rustos_fdt::Fdt`; their `rustos-util` dependency was
  swapped for `rustos-fdt`. `lib/util/src/dtb.rs` and `pub mod dtb` are
  removed (`lib/util` retains only `fmt`). The aarch64 IPI/SMP vertical's
  named `hvc` conduit is unaffected — that is QEMU's no-DTB-pointer ELF
  boot, not a parser gap, and the production path already discovers the
  conduit through `lib/fdt`.
- **Verified:** `lib/fdt` (17) and `rustos-drv-bus-mmio` (13) host tests
  pass; every migrated vertical compiles for `aarch64-unknown-none` and
  `riscv64gc-unknown-none-elf` (the `itest_*` cfgs under which the code is
  active). Docs: `docs/src/drivers/bus.md`, `lib/util` crate docs, `PLAN.md`,
  this file. No `lib/abi` change, so no ABI / C-header drift.

#### Stage W13 — cross-CPU TLB-shootdown HAL slice — ✅ landed

The last ad-hoc per-port memory primitive becomes a HAL trait. Local
per-page invalidation was already the `TlbShootdown` slice (W5b-2); on an
SMP system that only flushes the *calling* CPU, so a page-table edit that
tightens or tears down a shared mapping needs a system-wide shootdown.
W13 adds that as an object-safe `kernel/arch/api` trait and implements it
once per port.

- **`CrossCpuTlbShootdown` (`kernel/arch/api/src/xtlb.rs`).** One method,
  `shootdown_page(&self, vaddr)`, infallible by construction (a shootdown
  can only ever *over*-invalidate, never refuse — §2.9 holds vacuously).
  It is a separate trait from the local `TlbShootdown`, not a flag on it:
  the local flush is a single privilege-neutral instruction the hot
  map/unmap loop drives, whereas the cross-CPU shootdown needs the port's
  CPU topology and only returns once the invalidation is globally visible
  (collapsing them would be the §2.4 interface creep). Implemented on each
  port's `SchedulerArch` handle (the owner of topology + the directed-IPI
  path). Ships with a host `xtlb::conformance` vertical proving the
  observable half (object-safe, total, panic-free for any/zero/misaligned
  address), exactly as `tlb::conformance` does.
- **Per-port impls (the §2.2 modularity carve-out — same trait, port-
  specific mechanism):**
  - **x86_64** has no broadcast invalidation, so `kernel/arch/x86_64/src/
    tlb_shootdown.rs` raises a `TLB_SHOOTDOWN_VECTOR` (0x21) IPI at every
    other online CPU through a lock-serialised mailbox; each target runs
    `invlpg` in the shootdown ISR and decrements an acknowledge counter,
    and the initiator spins until the count hits zero. The local `invlpg`
    is the one already used by `TlbShootdown::flush_page` (shared, §2.2).
  - **aarch64** issues the inner-shareable *broadcast* `tlbi vaae1is` +
    `dsb ish`/`isb` — the same instruction the local flush already uses,
    so the local and cross-CPU paths funnel through one shared
    `paging::invalidate_page_inner_shareable` (§2.2); no IPI/ack needed.
  - **riscv64** has no broadcast `sfence.vma`, so it issues a local
    `sfence.vma` (shared `paging::invalidate_page_local`, §2.2) plus the
    SBI **RFENCE** `remote_sfence_vma` firmware call (new `sbi::sbi_call4`
    + `SBI_EXT_RFENCE`) to every other online hart; the firmware performs
    the remote acknowledge.
  - **wasm32** is an honest **n/a** — a Web Worker owns isolated linear
    memory with no shared page table or TLB — so it implements no
    `CrossCpuTlbShootdown` (§0.4 honest absence, never a faked no-op).
- **QEMU verticals (real ≥ 2 cores), one per bare-metal port, enrolled
  with `cpus: 2`:** `cross_cpu_tlb_shootdown_qemu_{riscv64,aarch64,x86_64}`
  each start a secondary CPU and drive `shootdown_page`. riscv64 asserts
  the firmware reports the remote fence reached the live hart; x86_64 only
  reaches PASS once the AP's ISR `invlpg`'d and acknowledged (the spin
  cannot return otherwise); aarch64 proves the broadcast executes on a
  real two-core machine without faulting.
- **Verified:** the three new host conformance tests
  (`passes_cross_cpu_tlb_shootdown_conformance`, one per port) pass; all
  three QEMU bins exit `0` under `cargo xtask test --qemu` (a single-CPU
  x86_64 run correctly *fails* — "no application processor found" — so the
  PASS is genuine). No `lib/abi` change, so no ABI / C-header drift. Docs:
  `docs/src/architecture/modularity.md`, `docs/src/platform/{x86_64,
  aarch64,riscv64,wasm32}.md`, `AGENTS.md` §17.2, `PLAN.md`, this file.

#### Stage W14 — SMP secondary-CPU bring-up HAL slice — ✅ landed

The last enumerated §17.2 primitive becomes a HAL trait. Secondary
bring-up was implemented port-side (W6 PSCI, W8 Web Worker, the riscv64
SBI HSM path, and the x86_64 INIT-SIPI-SIPI orchestration that lived in
the QEMU test bin), but it was not reachable through one neutral surface.
W14 adds it as an object-safe `kernel/arch/api` trait and folds each
port's existing `smp` onto it. The directed-IPI half of SMP is already
`SchedulerArch::send_ipi`, so this slice is **start-only** (§2.4 — no
interface creep).

- **`SecondaryBringup` (`kernel/arch/api/src/smp.rs`).** One method,
  `unsafe fn start_secondary(&self, cpu) -> Result<(), SmpError>`,
  implemented on each port's `SchedulerArch` handle (the owner of the
  dense `CpuId` ↔ native-id topology map). It **fails closed**
  (`SmpError::InvalidCpu`) before any platform action for the boot CPU,
  an out-of-range id, or an unmapped id, and never panics (§2.9). The
  set-once *entry* a fresh CPU runs is deliberately **not** on the trait
  (a bare-metal `extern "C" fn(CpuId) -> !` vs. wasm32's fixed module
  export — forcing one shape would make wasm32 fake the other, §2.1).
  Ships with a host `smp::conformance` vertical proving the observable
  half (object-safe, fail-closed, panic-free).
- **Per-port impls (the §2.2 carve-out — same trait, port mechanism):**
  - **x86_64** owns a per-AP stack pool, the `AP_TRAMPOLINE_PHYS` frame,
    the boot `CR3`, a PIT `Delay`, and a set-once entry slot;
    `smp::start_secondary` installs the trampoline, stamps each
    `ApBootSlot`, runs the SDM INIT-SIPI-SIPI handshake, and waits on the
    AP `ready` flag. This orchestration **moved out of**
    `scheduler_stress_qemu` and `cross_cpu_tlb_shootdown_qemu_x86_64`
    into the arch crate (§2.2 — the two verticals had duplicated it);
    both now call `X86_64Arch::start_secondary`.
  - **aarch64** delegates to the W6 `smp::start_secondary` (PSCI
    `CPU_ON`); the conduit is installed on the handle via
    `Aarch64Arch::with_psci_method` (a missing conduit → `NotReady`).
  - **riscv64** delegates to the SBI HSM `hart_start` `smp::start_secondary`.
  - **wasm32** delegates to `smp::start_worker` (Web Worker spawn); it
    has no settable entry, so it never reports `NotReady`.
- **Verified:** the four host `passes_secondary_bringup_conformance`
  tests pass; the migrated `scheduler_stress_qemu` (BSP starts 3 APs via
  the HAL) and `cross_cpu_tlb_shootdown_qemu_x86_64` build freestanding
  and stay green under `cargo xtask test --qemu`. No `lib/abi` change, so
  no ABI / C-header drift. Docs:
  `docs/src/architecture/modularity.md`, `docs/src/platform/{x86_64,
  aarch64,riscv64,wasm32}.md`, `AGENTS.md` §17.2, `PLAN.md`, this file.

#### Stage W15 — fold the bare-metal/wasm SMP verticals onto the HAL — ✅ landed

W14 routed the x86_64 SMP verticals (`scheduler_stress_qemu`,
`cross_cpu_tlb_shootdown_qemu_x86_64`) through
`SecondaryBringup::start_secondary` but left the
`ipi_smp_qemu_{riscv64,aarch64}` and `kernel_arch_boot_wasm32` verticals
calling the port-private `smp::start_secondary` / `smp::start_worker`
directly. That was sound (the free port helper the HAL delegates to is
not §2.2 duplication) but asymmetric. W15 finishes the symmetry: every
SMP vertical now starts its secondary through the neutral HAL trait, so
the bring-up surface the tests exercise matches the one the kernel uses.

- **riscv64** (`ipi_smp_qemu_riscv64`): the `RiscvArch::with_harts`
  handle is built up front and `arch.start_secondary(secondary_hartid)`
  replaces the bare `smp::start_secondary(hartid)`.
- **aarch64** (`ipi_smp_qemu_aarch64`): the `Aarch64Arch::with_cpus`
  handle now also carries `.with_psci_method(VIRT_PSCI_METHOD)` so
  `arch.start_secondary(SECONDARY_CPU)` can issue PSCI `CPU_ON`.
- **wasm32** (`kernel_arch_boot_wasm32`): `arch.start_secondary(WORKER_CPU)`
  on the existing `WasmArch::with_workers` handle replaces
  `smp::start_worker`.
- Each vertical keeps `smp::set_secondary_entry` (entry install is
  off-trait by design, §2.4) and imports `SecondaryBringup`. No new HAL
  surface, no `lib/abi` change → no ABI / C-header drift.
- **Verified:** `ipi_smp_qemu_{riscv64,aarch64}` exit `0` under
  `rustos-qemu-run --cpus 2`; the wasm32 browser harness reports
  `WORKER_OK=true IPI_RECV=true PASS`; whole-project `cargo fmt --all
  --check`, `cargo xtask ci`, `cargo xtask fuzz --secs 5`, and
  `tools/ci/soak.sh both --secs 10` are all green. Docs:
  `docs/src/architecture/modularity.md`, `docs/src/platform/{aarch64,
  riscv64,wasm32}.md`, `PLAN.md`, this file.

#### Stage W16 — wasm32 framebuffer display vertical (browser canvas) — ✅ landed

The last `display`-row parity gap. wasm32 had no `display` vertical; W16
adds the browser analogue of `framebuffer_display_qemu_{riscv64,aarch64}`
so every Tier-1 target now exercises the signed framebuffer-driver
lifecycle end-to-end against a real display surface.

- **New host import (`rustos_host_present_framebuffer`).** One import
  added to `kernel/arch/wasm32/src/bindings.rs` (+ safe wrapper
  `host_present_framebuffer`) and supplied by `web/rustos.js`
  (`makeEnv` + a `boot`/`runWorker` `presentFramebuffer` ctx hook,
  defaulting to a headless no-op). It is the wasm32 scan-out analogue of
  a bare-metal port reading its framebuffer back through an independent
  mapping: the host paints the presented RGBA8888 surface onto a canvas,
  reads it back, and returns the count of pixels that survived the
  round-trip. No `lib/abi` change → no ABI / C-header drift.
- **New vertical (`tests/integration/framebuffer_display_wasm32`).** A
  `cdylib` (host build inert, like the bare-metal stubs) that, on the
  boot context: loads the build-time signed framebuffer `.rxe` through
  `rustos_drvhost::Host` (the §8 gate) and drives `load → use → unload →
  reload`. "Use" maps the surface through a capability-checked
  `WasmMmioMapper` (the wasm32 MMU-less analogue of `KernelMmioMapper` —
  a bounds- + `CAP_MMIO_MAP`-gated view of the one in-memory surface) and
  `present`s a frame, confirmed **twice**: through a second,
  independently-mapped window (bytes reached linear memory) and through
  the canvas round-trip (`host_present_framebuffer` returns all
  `WIDTH×HEIGHT` pixels). Prints `BOOT_OK` then `DISPLAY_OK`; every
  failure traps the instance (`AGENTS.md` §2.9 / §5.4.5). Uses
  `DisplayFormat::Rgba8888` with opaque (`0xFF`) alpha so the canvas
  premultiplied-alpha round-trip is lossless.
- **Harness generalised (§2.2).** `web/index.html` supplies the canvas
  `presentFramebuffer` hook; `web/harness.mjs` is the boot harness's
  sibling (PASS on `BOOT_OK`+`DISPLAY_OK`). `tools/xtask/.../wasm_tests.rs`
  now drives a `VERTICALS` list (boot + display) instead of one
  hard-coded package, so `cargo xtask test --wasm` builds and runs both;
  adding a wasm32 vertical is one row there.
- **Verified:** `cargo xtask test --wasm` is browser-green — the boot
  vertical (`BOOT_OK ISOLATION_OK WORKER_OK IPI_RECV ticks=20 PASS`) and
  the new display vertical (`BOOT_OK=true DISPLAY_OK=true PASS`).
  Whole-project `cargo fmt --all --check`, `cargo xtask ci`, `cargo xtask
  fuzz --secs 5`, and `tools/ci/soak.sh both --secs 10` all green. Docs:
  `docs/src/platform/wasm32.md`, `docs/src/drivers/display.md`,
  `PLAN.md`, this file.

#### Stage W17 — one trimmed aarch64 `virt` DTB embed (§2.2) + close the lib/fdt-runtime-parse note — ✅ landed

Resolves the long-standing "`lib/fdt` runtime parse of the full ARM
`virt` tree" carry-forward (W6/W7) and the §2.2 duplication the
DTB-embedding device verticals had grown.

- **The note was stale.** W12 gave `lib/fdt` the full `virt`-tree node
  API, and the W11 device verticals (`virtio_blk/net_mmio_aarch64`,
  `framebuffer_display`, `input_virtio_mmio`) already **parse the full ARM
  `virt` tree at runtime** through `rustos_fdt::Fdt` (e.g.
  `device_spi_number` walks `virtio,mmio` `reg`/`interrupts`; the display
  vertical reads the `fw_cfg` base) — after their EL1 identity-MMU
  bring-up. So runtime full-tree parsing on aarch64 is already proven;
  what remained was only the *naming* of the PSCI conduit in the two SMP
  verticals.
- **Why the SMP verticals legitimately name the conduit (honest §0.4
  constraint).** `ipi_smp_qemu_aarch64` and
  `cross_cpu_tlb_shootdown_qemu_aarch64` run **MMU-off by design** (they
  exercise only secondary bring-up / IPI / the inner-shareable TLB
  broadcast, and install no exception vectors on the boot core). With the
  stage-1 MMU disabled every access is Device memory, where the FDT
  walk's compiler-emitted multi-byte loads fault; with no vectors yet
  installed that hangs the core. Forcing an identity MMU + vectors into
  those minimal verticals purely to re-derive the constant `hvc` would
  distort what they prove and duplicate the bring-up a third time
  (§2.1/§2.3). They therefore keep naming the board conduit directly —
  test-environment knowledge on par with the fixed two-core MPIDR layout,
  exactly as their module docs state — and the production discovery path
  (`fdt::psci_method`) stays host-tested + conformance-gated (W1).
- **§2.2 consolidation that did land.** The four aarch64 device build
  scripts had four byte-identical `dump_virt_dtb` copies. They now reuse a
  single build-glue helper, `rustos_itest_harness::dump_aarch64_virt_dtb`
  (with the unit-testable `dump_virt_dtb_args`), so the
  `qemu ... dumpdtb` invocation lives in one audited place.
- **Trimmed embed (image size).** `dumpdtb` pads the blob to the
  machine's 1 MiB device-tree region; `trim_fdt_to_extent` now trims it to
  the extent its FDT header describes and rewrites `totalsize`, so each
  device vertical embeds the few-KiB meaningful tree instead of ~1 MiB of
  zero padding. The trimmed blob stays a valid FDT (`rustos_fdt::Fdt::new`
  validates against the buffer length, not `totalsize`), proven by a
  harness round-trip unit test over the shared `rustos_fdt::fixture`
  builder and by the device verticals parsing it at runtime.
- **Verified:** the four migrated aarch64 device verticals build
  freestanding; `framebuffer_display_qemu_aarch64` (ramfb) and
  `virtio_blk_mmio_aarch64` are QEMU-green against the trimmed DTB; the
  SMP verticals are unchanged and stay QEMU-green; whole-project
  `cargo fmt --all --check`, `cargo xtask ci`, `cargo xtask fuzz --secs 5`,
  and `tools/ci/soak.sh both --secs 10` are all green. No `lib/abi`
  change → no ABI / C-header drift. Docs:
  `docs/src/platform/aarch64.md`, `PLAN.md`, this file.

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
