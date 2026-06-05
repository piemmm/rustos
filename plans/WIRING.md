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
(allocator-backed port tables via `FrameTableSource` — W5b-3). The
remaining slices (cross-CPU TLB shootdown; SMP bring-up — landed
port-side per arch in W6, not yet a HAL trait) are still ad-hoc inside
each port — the §17.2 surface `PLAN.md` flags as "migrated here as the
§17 burn-down advances".

**Parity matrix** (✓ present, ~ partial, ✗ missing, n/a not applicable):

| Capability / HAL slice            | x86_64 | aarch64 | riscv64 | wasm32 |
| --------------------------------- | :----: | :-----: | :-----: | :----: |
| Boot stub + early console         |   ✓    |    ✓    |    ✓    |   ✓    |
| `SchedulerArch` impl              |   ✓    |    ✓    |    ✓    |   ✓    |
| Paging / MMU primitives (`AddressSpace` HAL) | ✓ | ✓ | ✓ | n/a |
| Memory isolation QEMU vertical    |   ✓    |    ✓    |    ✓    |   ~    |
| Context switch (`ContextSwitch`)  |   ✓    |    ✓    |    ✓    |  n/a   |
| Timer + preemption HAL (`Timer`)  |   ✓    |    ✓    |    ✓    |   ✓    |
| Interrupt controller + entry/exit |   ✓    |    ✓    |    ✓    |  n/a   |
| **SMP secondary-CPU bring-up**    |   ✓    |    ✓    |    ✓    | **✗**  |
| **IPI delivery (real cores)**     |   ✓    |    ✓    |    ✓    | **✗**  |
| **Early-boot platform discovery** | ✓ ACPI | **✗** FDT| ✓ FDT  |  ~ JS  |
| Per-CPU storage HAL               |   ✓    |    ✓    |    ✓    |   ✓    |
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
| **`ipi_smp`**           | ✓(stress)|    ✓    |    ✓    |  **✗** |
| **`sched_drive`** (live)| ✓(stress)|  **✗**  |    ✓    |  **✗** |
| `enter_user`            |   ✓    |  ~(spawn)|  ~(spawn)|  **✗** |
| `irq`                   |   ✓    |    ✓    |    ✓    |  n/a   |
| input (`ps2`/device)    |   ✓    |  **✗**  |  **✗**  |  n/a   |
| display (`vesa`/fb)     |   ✓    |  **✗**  |    ✓    |  **✗** |
| `virtio` blk/net        | ✓(pci) |  **✗**  | ✓(mmio) |  n/a   |

**Headline gaps, ranked:**
- **aarch64:** SMP secondary-core bring-up + real IPI landed (W6); no
  live-scheduler task switch, no heterogeneous `core_class`, and missing
  input / display / virtio verticals. (FDT/DTB discovery is host-tested
  via W1; the runtime parse of the full ARM `virt` tree is still a gap.)
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
  into `kernel/mem` (map/translate/unmap + per-page local invalidation —
  ✅ W5b-2); allocator-backed port tables via the `PageTableFrames`
  frame-source seam (✅ W5b-3); cross-CPU TLB invalidation (W6) remains.
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
