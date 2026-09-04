# Modularity contracts and enforcement

`AGENTS.md` §17 guarantees that the scheduler, the architecture backend,
and the desktop can each be replaced or omitted without rewriting the
rest of the system. Those guarantees are not honour-based: two `xtask`
subcommands fail the build when the workspace drifts from them, and both
run inside `cargo xtask ci`.

## `cargo xtask deps-check`

Reconstructs the workspace dependency graph from the member manifests —
every in-workspace edge is a `path =` dependency, so no external crate is
needed to read it — and rejects three classes of defect:

- **Layering (§17.4).** Each crate is classified into a stratum from its
  directory (`lib`, the Arch HAL api/impl, the scheduler api/impl, kernel
  subsystems, `kernel/core`, drivers, userland, and the GUI). An edge is
  permitted only if the source stratum is allowed to depend on the target
  stratum. `lib/*` may depend only on `lib/*`; drivers and non-GUI
  userland may depend only on `lib/*`; only `kernel/core` may name a
  concrete architecture or scheduler crate.
- **Concrete-scheduler naming (§17.1).** A kernel crate outside
  `kernel/core` and `kernel/sched/*` may not name a concrete scheduler
  crate; the rest of the kernel depends on the policy trait instead.
- **Optional desktop (§17.3).** No non-GUI crate may reach any
  `userland/gui/*` crate, even transitively. This edge is checked with no
  exceptions: the desktop boundary is clean and must stay clean.

Only build-graph dependencies are considered; `[dev-dependencies]` are
test scaffolding and are excluded.

### Grandfathered edges

Offending edges that predate §17 are pinned in an explicit, commented
allow-list inside the checker. The list is append-never — it may only
shrink — and a *new* violating edge is always rejected. Each pinned edge
is a tracked defect scheduled for the §17 burn-down in `PLAN.md`.

That list is now empty: the layering is satisfied with no exceptions. The
final edges were the x86_64 production binary's bring-up dependencies
(`tairix-kernel → kernel/core`, the arch port, the driver host, and the
boot-time bus driver). `tairix-kernel` is the image-assembly seam, not a
kernel subsystem, so it is classified as `Tooling` (outside the product
layering) rather than grandfathered — the x86_64 analogue of the
downstream `tests/integration/riscv64_boot` consumer, which wires the
riscv64 image together the same way.

## The Arch HAL (`kernel/arch/api`)

The §17.2 architecture surface lives in its own crate, `kernel/arch/api`
(`tairix-arch-api`). It is `no_std` and names a single `lib/*`
dependency — `tairix-abi`, itself `no_std`, dependency-free, and
allocator-free — so the kernel can name the HAL without inheriting an
architecture, and a port can implement the HAL without naming a concrete
kernel crate — the two sides meet only here. Today the crate carries the
scheduler-facing slice (`CpuId`, `SchedulerArch`), the §19.1
side-channel slice, the §19.10 memory-tagging slice, the user-entry
slice, the early-boot platform-discovery slice (`PlatformDiscovery`,
below), the per-CPU storage slice (`PerCpu`, below), and the interrupt
entry/exit slice (`IrqController` + `InterruptEntry`, below), the timer
slice (`Timer`, below), the context-switch slice (`ContextSwitch`,
below), the MMU / page-table slice (`AddressSpace`) with its local and
cross-CPU TLB-shootdown siblings (`TlbShootdown` / `CrossCpuTlbShootdown`)
and the `PageTableFrames` frame source, and the secondary-CPU bring-up
slice (`SecondaryBringup`, below). With `SecondaryBringup` landed (Stage
W14) every architecture primitive enumerated by §17.2 now lives behind
the HAL; the burn-down is complete.

`kernel/arch/x86_64` implements `tairix_arch_api::SchedulerArch` for
`X86_64Arch` and no longer names a scheduler crate; `kernel/sched/api`
re-exports the HAL trait so existing `tairix_kernel_sched_api::SchedulerArch`
paths resolve to the single canonical definition. `kernel/arch/riscv64`
is likewise a pure HAL implementation: `RiscvArch` implements
`SchedulerArch`, and the boot orchestration that used to name concrete
kernel crates (the `BootInfo` assembly, the `KernelArch` wrapper, the
boot-state slots, and the `IrqController` bridge over its PLIC) moved
into the downstream Tooling crate `tests/integration/riscv64_boot` —
the riscv64 analogue of how x86_64 keeps that pipeline in `tairix-kernel`.
Both ports now name only `kernel/arch/api` + `lib/*`.

### Arch HAL conformance vertical

Parity between ports is *enforced*, not asserted by inspection
(`plans/WIRING.md` Stage W0). `kernel/arch/api` carries a
`conformance` module — the architecture analogue of the
`kernel/sched/api` policy conformance suite — written purely against the
HAL traits so it names no concrete port:

- `conformance::run_scheduler_arch(arch)` checks the `SchedulerArch`
  contract: `current_cpu` stable across back-to-back calls, `ticks_now`
  monotonically non-decreasing, `send_ipi` to self (and to a stray
  target) a panic-free no-op equivalent, and `core_class` total — it
  returns a stable, valid class for every `CpuId`, including an
  out-of-range one.
- `conformance::run_all(arch, side_channel, memory_tagging, discovery,
  per_cpu)` runs that suite **and** the §19.1 side-channel vertical
  (`sidechannel::conformance`), the §19.10 memory-tagging vertical
  (`memtag::conformance`), the §18 platform-discovery vertical
  (`platform::conformance`), and the per-CPU storage round-trip vertical
  (`percpu::conformance`) over the same port's handles. Each port
  implements the five traits on distinct types (`*Arch`, `SideChannel`,
  `MemoryTags`, a discoverer, and `PerCpuStorage`), so the suite takes
  one reference per trait.

### Early-boot platform discovery

The `PlatformDiscovery` slice normalises each target's native hardware
source into the single `lib/abi` hardware tree (`hwtree`, §18.1/§18.2):
x86_64 reads the ACPI MADT (`AcpiDiscovery`), riscv64 and aarch64 read a
flattened device tree (`FdtDiscovery`, on the shared `lib/fdt` parser),
and wasm32 queries the JavaScript host (`HostCapabilityDiscovery`). A
discoverer pushes root-first `HwNode`s into a caller-owned `HwNodeSink`,
so the trait chooses no allocator (the kernel collects on the stack, the
device manager into a growable buffer). `platform::conformance::run`
asserts the contract — at least one node, exactly one root, unique ids,
every non-root parent emitted before its child, every device class
decodable, and a full sink surfaced rather than silently dropped.

The api crate's own `tests/conformance.rs` drives the harness over an
in-test double (it cannot name a concrete port without inverting the
§17.4 layering). The real per-port coverage lives in each
`kernel/arch/<target>` crate's host test
`kernel_arch::tests::passes_arch_hal_conformance_suite`, which
instantiates `conformance::run_all` over its real handles. All four
Tier-1 ports pass.

### Per-CPU storage

The `PerCpu` slice reads and writes the calling CPU's per-CPU base word —
the lock-free anchor the kernel resolves its CPU-local state from. Each
port drives the native mechanism behind the one trait: x86_64 the GS-base
MSR (`IA32_GS_BASE`), aarch64 `TPIDR_EL1`, riscv64 the `tp` thread
pointer, and wasm32 a worker-local slot (each Web Worker owns its own
module instance, so the slot is private to it without host coordination).
The stored word is opaque to the trait — the kernel chooses whether it
holds a per-CPU control-block address or a dense `CpuId`.
`percpu::conformance::run_all` asserts the word round-trips unchanged at
full pointer width (folded into `conformance::run_all`), and
`percpu::conformance::run_isolation` asserts one CPU's word is
independent of another's — driven over two handles, since a single
handle cannot express the per-CPU property.

### Interrupt entry/exit

The interrupt slice gives the kernel one vocabulary for the programmable
interrupt controllers that differ entirely at the register level — the
x86_64 IO-APIC, the aarch64 GICv2, the riscv64 PLIC. `IrqController`
masks and unmasks a controller line (the load-bearing mask-before-wake
primitive of the user-space IRQ contract, `docs/src/security/irq.md`),
validating every line and failing closed (`IrqControlError::OutOfRange`)
on a stray one. `InterruptEntry` is the claim → complete prologue/
epilogue a *claim-based* controller exposes: riscv64 maps it onto the
PLIC claim/complete pair and aarch64 onto the GICv2 `IAR`/`EOIR`
handshake (spurious reads map to `None`). x86_64 is **vectored** — the
IDT vector already identifies the source and end-of-interrupt is a single
LAPIC write that names no line — so it implements `IrqController` only
and deliberately omits `InterruptEntry`; inventing a claim register it
lacks would be a fake primitive (`AGENTS.md` §2.1). Each port wraps its
MMIO behind a seam (`PlicMmio` / `GicMmio` / `IoApicMmio`) so the whole
controller is host-testable, and drives
`irq::conformance::run_controller` (mask/unmask round-trip + fail-closed)
and, where applicable, `irq::conformance::run_entry` (claim/complete
drain terminates) over its real handle. These verticals are driven
per-port rather than folded into `conformance::run_all`: the controller
check needs a port-specific valid/invalid line pair and `InterruptEntry`
is implemented by only a subset of ports — the same reason
`percpu::conformance::run_isolation` stands apart.

### Timer programming

The `Timer` slice gives the kernel one vocabulary for the periodic
per-CPU scheduler tick, whose hardware differs entirely per target: the
x86_64 LAPIC timer, the aarch64 EL1 physical generic timer
(`CNTP_*_EL0` + GIC PPI 30), the riscv64 supervisor (SBI) timer, and the
wasm32 cooperative `requestAnimationFrame` loop. `Timer::set_tick_callback`
installs the one architecture-neutral scheduler-tick callback
(`extern "C" fn(CpuId)`), and `Timer::dispatch_tick` invokes it on a
tick — the shared half of every port's interrupt/frame handler, so the
callback invoke lives in exactly one place (`AGENTS.md` §2.2). The
*hardware* arming/re-arming (programming the LAPIC LVT, `CNTP_TVAL_EL0`,
the SBI timer, or requesting the next frame) stays in the port's
`preempt` module — it is per-CPU register/MMIO work with no
architecture-neutral shape, and folding it into the trait would be
interface creep (`AGENTS.md` §2.4). Each port exposes a `TimerHal`
handle and the riscv64/aarch64/wasm32 tick handlers dispatch back
through it; x86_64's vectored ISR must read the LAPIC ID and issue the
EOI itself, so it keeps its own dispatch and `TimerHal` is its
HAL-facing surface. `timer::conformance::run_all` asserts an installed
callback fires on dispatch with the CPU it was handed and that a handle
with no callback dispatches harmlessly. Like the interrupt verticals it
is driven per-port (the handle is constructed per port and reaches a
port-private callback slot), not folded into `conformance::run_all`.

### Context switch

The `ContextSwitch` slice gives the kernel one vocabulary for suspending
the running task and resuming another, whose register save/restore is
deeply architecture-specific assembly (x86_64 `context.s`, aarch64
`x19`–`x30`, riscv64 `ra`/`s0`–`s11`). Every bare-metal port persists
exactly one word across a switch — the kernel-stack pointer at
suspension, with the callee-saved registers parked on the task's own
stack in a fixed frame the switch assembly owns — so the
architecture-neutral save area `TaskContext` is a single `#[repr(C)]`
`u64`, layout-identical to each port's native `TaskCtx` (one definition,
`AGENTS.md` §2.2; a const-assert in each port pins the equality).
`ContextSwitch::prepare` seeds a never-run task's first frame (rejecting
a null/misaligned/too-small stack fail-closed, `AGENTS.md` §2.9) and
`ContextSwitch::switch` performs the bare-metal switch. Each bare-metal
port exposes a `ContextSwitchHal` handle that reinterprets `TaskContext`
as its `TaskCtx` and forwards to the existing `context` primitive, so the
switch invoke lives in exactly one place. wasm32 has no context switch:
each Web Worker is its own sandboxed module instance and the kernel never
swaps register state under it, so the slice is **n/a** there (`AGENTS.md`
§2.1 — no fake primitive). `context::conformance::run_all` asserts the
`prepare` contract on the host (an empty context is not runnable; a
null/misaligned/too-small stack is rejected; a good stack yields a
runnable, in-bounds frame); like `EnterUser`, the switch itself is
proven only on the bare-metal target (the scheduler-drive QEMU vertical),
so it carries no host check. The vertical is driven per-port (it seeds a
frame over a caller-supplied stack and runs over the port's real handle),
not folded into `conformance::run_all`.

### MMU / page-table

The `AddressSpace` slice (`kernel/arch/api::mmu`) gives the kernel one
vocabulary for the page-table primitive, whose format differs entirely
per target: the x86_64 four-level PML4 loaded into `CR3`, the riscv64
three-level Sv39 hierarchy selected by `satp`, and the aarch64
three-level stage-1 table programmed into `TTBR0_EL1` with `SCTLR_EL1.M`.
`AddressSpace::map_page` installs a 4 KiB mapping over the neutral
`PageFlags` permission set (`READ`/`WRITE`/`EXEC`/`USER`/`DEVICE`, decoded
once at the HAL boundary into native PTE bits — one vocabulary, `AGENTS.md`
§2.2), failing closed (`Misaligned`/`AlreadyMapped`/`PoolExhausted`/
`InvalidFlags`, `AGENTS.md` §2.9) and rejecting a W^X-violating write+exec
leaf (`AGENTS.md` §19.2). `AddressSpace::translate` is the read-only
inverse — a walk that decodes a leaf back to its physical page and
`PageFlags`, or `None` — and `AddressSpace::unmap` tears a 4 KiB leaf
down and returns its frame (failing closed with `NotMapped` on an absent
or large-page address). `AddressSpace::root_phys` reports the root-table
physical address, and `AddressSpace::activate` makes the space the live
translation regime (and performs the port's coarse TLB flush). Each port
implements the trait on its existing `paging::AddressSpace`, translating
`PageFlags` to and from native leaf attributes and forwarding `activate`
to its gated `switch` primitive, so the page-table walk and the register
write each live in exactly one place.

`mmu::conformance::run_all` asserts the full map lifecycle on the host
(a non-null root; misaligned addresses rejected; a good mapping accepted
and then translating back to its frame; a double mapping refused; an
unmap returning the frame and then translating to nothing; a second
unmap failing closed), driven per-port — the suite needs a
port-constructed address space and a port-specific mappable address pair,
the same reason the `irq`/`timer` verticals stand apart. riscv64 and
aarch64 run it over their real `AddressSpace` (their walk recovers tables
through the identity map, so it is host-runnable); x86_64's walk reaches
intermediate tables through the higher-half kernel window (phys ≠ virt),
so it is not host-runnable and its `map_page`/`activate` are proven by the
`memory_isolation` QEMU vertical instead — like the bare-metal `switch`,
which never carries a host check (`AGENTS.md` §2.1 — no fake primitive).
wasm32 has no page table (each Web Worker is a sandboxed linear-memory
instance the kernel never re-maps), so the slice is **n/a** there. The
three `memory_isolation_qemu_*` verticals build their victim/attacker
spaces through this trait, so the §4 "isolation is enforced by hardware"
property is proven *through the HAL*.

### TLB shootdown

The `TlbShootdown` slice (`kernel/arch/api::tlb`) gives the kernel one
name for per-CPU single-page TLB invalidation, whose instruction differs
per target: x86_64 `invlpg`, aarch64 `tlbi vaae1is` (with `dsb`/`isb`
barriers), riscv64 `sfence.vma`. `kernel/mem`'s per-process map/unmap
path calls `TlbShootdown::flush_page` after editing a leaf so the next
access re-walks the updated table. A flush only ever *discards* cached
state, so it is infallible by construction; `tlb::conformance::run_all`
proves the observable half on the host (object-safe, panic-free for any
address, including misaligned and zero) for every port — the real
instruction is exercised by the spawn / `memory_isolation` QEMU
verticals.

### Cross-CPU TLB shootdown

The local `TlbShootdown` only flushes the *calling* CPU; on an SMP
system a page-table edit that tightens or tears down a shared mapping
must also invalidate every other CPU's cached translation. The
`CrossCpuTlbShootdown` slice (`kernel/arch/api::xtlb`) is that
system-wide operation: one infallible method,
`shootdown_page(&self, vaddr)`, implemented on each port's
`SchedulerArch` handle (the owner of the CPU topology and the
directed-IPI path). It is a *separate* trait from the local flush, not a
flag on it — collapsing the cheap per-edit local flush and the expensive
system-wide shootdown into one call would be the §2.4 interface creep —
and it can only ever *over*-invalidate, so like the local flush it is
infallible by construction (§2.9 holds vacuously).

The mechanism is the §2.2 modularity carve-out (same trait, port-specific
implementation): x86_64 has no broadcast invalidation, so it raises a
shootdown IPI at every other online CPU through a lock-serialised
mailbox and spins until each target's ISR has run `invlpg` and
acknowledged; aarch64 issues the inner-shareable broadcast `tlbi vaae1is`
+ `dsb ish`/`isb` (the *same* instruction the local flush uses, so both
paths funnel through one shared `paging::invalidate_page_inner_shareable`
helper); riscv64 issues a local `sfence.vma` plus the SBI RFENCE
`remote_sfence_vma` firmware call to every other hart; wasm32 is an
honest **n/a** (a Web Worker owns isolated linear memory with no shared
TLB) and implements nothing. `xtlb::conformance::run_all` proves the
observable half on the host (object-safe, panic-free for any address);
the real cross-CPU round-trip is proven by the three
`cross_cpu_tlb_shootdown_qemu_*` QEMU verticals on real ≥ 2 emulated
cores (`plans/WIRING.md` Stage W13).

### Secondary-CPU bring-up

`AGENTS.md` §4 mandates SMP from day one, so the kernel must start the
machine's other logical CPUs. That was the last enumerated §17.2
primitive still ad-hoc per port: each port owned a `smp` module, but the
rest of the kernel could not start a CPU through one neutral surface. The
`SecondaryBringup` slice (`kernel/arch/api::smp`) closes it: one method,
`unsafe fn start_secondary(&self, cpu: CpuId) -> Result<(), SmpError>`,
implemented on each port's `SchedulerArch` handle (the owner of the dense
`CpuId` ↔ native-id topology map). The *directed-IPI* half of SMP already
lives on `SchedulerArch::send_ipi`, so this slice is only about
**starting** a parked CPU and does not duplicate the IPI surface (§2.4).
The call **fails closed** (`SmpError::InvalidCpu`) before any platform
action for a CPU it cannot start — the boot CPU, an out-of-range id, or
one absent from the topology — and never panics (§2.9).

The set-once *entry* a fresh CPU runs is deliberately **not** on the
trait. On the bare-metal ports it is an `extern "C" fn(CpuId) -> !`
installed once via the port's `set_secondary_entry`; on wasm32 a
secondary is a fresh module instance entering at a fixed export, not a
runtime pointer one instance can hand another. Forcing a settable-entry
method onto the HAL would make wasm32 fake one it could never honour
(§2.1), so entry installation stays the genuinely port-shaped concern it
is, performed once before the first `start_secondary`.

The mechanism is the §2.2 modularity carve-out (same trait, port
mechanism): **x86_64** owns a low-memory trampoline, a per-AP stack pool,
and the boot PML4 — `start_secondary` installs the trampoline, stamps the
per-AP `ApBootSlot`, runs the SDM INIT-SIPI-SIPI handshake, and waits on
the AP's long-mode `ready` flag before returning (so the shared frame is
reusable); **aarch64** issues PSCI `CPU_ON` over the device-tree conduit
(`hvc`/`smc`) targeting the CPU's `MPIDR_EL1`; **riscv64** issues the SBI
HSM `hart_start` firmware call targeting the hart id; **wasm32** asks its
JavaScript host to spawn a Web Worker. `smp::conformance::run_all` proves
the observable half on the host (object-safe, fails closed for an
unstartable id, never panics) for every port — the real cross-core
bring-up is proven by the multi-core `scheduler_stress_qemu`,
`ipi_smp_qemu_*`, and `cross_cpu_tlb_shootdown_qemu_*` QEMU verticals on
real ≥ 2 emulated cores, and the wasm32 browser vertical
(`plans/WIRING.md` Stage W14). Every one of those verticals — across all
four ports — starts its secondary through `start_secondary`, not the
port-private `smp` helper (`plans/WIRING.md` Stage W15). With this slice
every §17.2 primitive listed by `AGENTS.md` is behind the HAL.

### Page-table frame source

A port's `AddressSpace` is built from 4 KiB page-table frames — the root
table and every intermediate table a mapping walk allocates. The
`PageTableFrames` slice (`kernel/arch/api::frames`) is the seam a port
draws those frames through: `alloc_table` hands back a `TableFrame`
carrying both the frame's physical address (for the parent PTE / root
register) and a zeroed `'static` view of its 512 entries. A port never
owns the storage and never computes the physical/virtual relationship
itself; the source does. This keeps the §17.4 one-way edge intact — a
port names only the HAL trait, never `kernel/mem` — while letting the
caller decide where the frames come from.

There are two implementations of the one trait (parallel impls, not
duplication, `AGENTS.md` §2.2 carve-out). The static `PageTablePool`
each port ships is the boot/bootstrap source. The production source is
`kernel/mem`'s `FrameTableSource`, which draws a physical frame from the
buddy `FrameAllocator`, maps it through the kernel direct map
(`PhysMap`), zeroes it, and fails closed — returning the frame to the
allocator — if the frame falls outside the direct map (`AGENTS.md`
§2.9). `frames::conformance::run_all` proves the contract on the host
(a fresh frame is zeroed, page-aligned, distinct from earlier frames,
and the source eventually fails closed with `None`): riscv64 and aarch64
run it over their real `PageTablePool` (their `phys_of` is the identity
map, so it is host-runnable) and `kernel/mem` runs it over
`FrameTableSource`; x86_64's pool derives `phys` by subtracting the
higher-half base, so its pool is proven through the `memory_isolation`
QEMU vertical instead (the same honest asymmetry the MMU slice carries).

This is the `plans/WIRING.md` Stage W5b lineage: Stage W5b-1 lifted the
bootstrap page-table primitive behind the HAL; Stage W5b-2 folded
`kernel/mem`'s `AddressSpace` onto the `mmu::AddressSpace` +
`TlbShootdown` traits (removing its local `PageTableOps` trait) and added
the `translate`/`unmap`/`flush_page` surface and the per-page TLB
shootdown; Stage W5b-3 added the `PageTableFrames` frame-source seam and
the `FrameTableSource` allocator backing above, so a real per-process
address space's tables come from the frame allocator while the static
`PageTablePool` stays the boot/bootstrap source. The cross-CPU shootdown
(`CrossCpuTlbShootdown`, Stage W13) is a sibling HAL slice described
above, not duplicated here (`AGENTS.md` §2.2).

## `cargo xtask cfg-check`

Scans every workspace `.rs` file and fails if a `cfg` predicate names
`target_arch` or `target_pointer_width` outside the allow-list of §17.2:
the architecture ports under `kernel/arch/<target>/` and the build glue
(`.cargo/`, `tools/mkimage/`, `tools/xtask/`). Target-conditional code
anywhere else means the Arch HAL boundary has leaked. As with
`deps-check`, any directory that violates the rule today is listed in a
shrink-only grandfather set; that set is currently empty — no workspace
source names the target instruction set outside the allow-list.

### Freestanding integration-test harness

The freestanding QEMU integration binaries under `tests/integration/`
compile two ways: as bare-metal `no_std`/`no_main` kernels for a QEMU
target, and as inert host stubs for `cargo build --workspace`. Choosing
between those forms is a target decision, so it cannot live in the test
source — that would name the instruction set outside the architecture
ports.

Instead it lives in one audited build-glue crate,
`tests/integration/harness` (`tairix-itest-harness`). Each test crate
calls `tairix_itest_harness::emit_target_cfg()` from its build script;
the helper inspects the cargo target and enables custom cfgs:

- `freestanding` — a bare-metal (`os = "none"`) target; compile the
  kernel body.
- `itest_x86_64` / `itest_riscv64` — the freestanding x86_64 / riscv64
  ports.

Every binary and the shared `virtio_qemu_support` library gate on those
names (`#[cfg(itest_x86_64)]`, `#[cfg(not(itest_x86_64))]`, …) rather
than on `cfg(target_arch …, target_os = "none")`, so `cfg-check` scans
the tree with no grandfather entry for it.

The harness also owns the rest of each vertical's build glue, so a
fixture's layout and its generated source have one definition rather than
a copy per crate: `program_fixture::PROGRAM_LD` is the single PIE link
script every fixture *program* links with, and `fixture_header` /
`push_rxe_blob` / `write_fixture` emit the `USER_BIAS` + image-bytes
source each vertical `include!`s. The bias they emit is the one
`USER_IMAGE_BIAS` every `rxe` converter bakes relocations for, rendered as
a grouped hex literal because generated source is linted like any other.

That harness is a *build* dependency, so nothing it holds can be linked
by a running test kernel. Logic the kernel bodies themselves share needs
a second crate, `tests/integration/finisher`
(`tairix-itest-finisher`): `no_std`, dependency-free, and a runtime
dependency of each freestanding binary. It owns the finisher-code
vocabulary: `fail_point!(n)`, which mints the code naming one of a
fixture's failure points, and `fail_code`, which composes the code a
fixture reports from its failure base and an observed value — a child's
exit code, or the CPU mask a migration test saw.

A finisher code is a `NonZeroU16`, because both boards read a zero code
as *success*: aarch64 passes it as the semihosting `SYS_EXIT` subcode and
riscv64 packs it into the `sifive_test` high half, so a zero-coded
failure exits QEMU with status 0 and the runner reads the failing run as
a pass. `qemu_exit::exit_failure` therefore takes a `NonZeroU16` rather
than checking one, `fail_point!` rejects a zero literal at compile time,
and `fail_code` cannot compose one.

The composition is there because it must be total and cannot be tested
where it was written. The observed value comes from the program under
test, so a fixture that panicked or wrapped while reporting it would turn
a real failure into a debug abort or another failure's code. A fixture
body compiles only for its bare-metal target, where no host test can
reach it; in the shared crate the property is proven once by ordinary
unit tests.

### Freestanding production kernel binary

The `tairix-kernel` crate has the same two-form shape: a bare-metal
`no_std`/`no_main` kernel for `x86_64-unknown-none` and an inert host
stub for `cargo build --workspace` / `cargo test`. Choosing between them
is a target decision, so it lives in the crate's build glue rather than
in the source. `kernel/tairix-kernel/build.rs` derives the bare-metal
condition from `CARGO_CFG_TARGET_OS`/`CARGO_CFG_TARGET_ARCH` and emits a
single custom `freestanding` cfg (declared with `rustc-check-cfg`). The
crate gates its `#![no_std]`/`#![no_main]` attributes, the
`boot`/`panic_ctx`/`serial_sink` modules, the IO-APIC typed publication
slot, and the fail-closed `halt` on `#[cfg(freestanding)]` rather than
`cfg(all(target_arch = "x86_64", target_os = "none"))`, so the target
choice stays in the one audited build-glue file.

## Headless builds

`cargo xtask build --headless` excludes every `userland/gui/*` crate from
the image, exercising the first-class headless configuration required by
§17.3. The headless image must build for every Tier-1 target.
