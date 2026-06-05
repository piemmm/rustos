# aarch64

RustOS targets `aarch64-unknown-none` as a Tier-1 platform. Stage 3b
delivers the QEMU `virt`-board boot and Arch-HAL primitives for the
64-bit Arm port: an EL1 boot trampoline, a PL011 UART console, the
`Aarch64Arch` implementation of the Arch HAL, the EL1 exception vector
table, a GICv2 driver, generic-timer preemption, the stage-1 MMU
primitives, the `svc` syscall-entry marshalling, and the ARM semihosting
test finisher. This page documents the boot model, the result protocol,
the arch primitives, and the QEMU argv contract.

## Arch HAL boundary

Like x86_64 and riscv64, `kernel/arch/aarch64` is a pure Arch HAL
implementation (`AGENTS.md` §17.2 / §17.4): it implements
`rustos_arch_api::SchedulerArch` (`Aarch64Arch`) plus the monotonic
clock and a CPU-park primitive, and names only `kernel/arch/api` and
`lib/*` — never a concrete kernel subsystem. A downstream boot consumer
wraps `Aarch64Arch` in its own `kernel_core::KernelArch` adapter.

The freestanding-only modules (`boot.s`, `serial`, `panic`, `entry`,
the exception/GIC/timer/MMU MMIO and system-register operations) are
gated to `cfg(all(target_arch = "aarch64", target_os = "none"))`; every
pure bit/encoding/layout helper (the `Aarch64Arch` struct, the paging
descriptors, the context `prepare`, the syscall arg marshalling, the
generic-timer interval math, the GIC register encoders, the semihosting
finisher constants) builds on the host so its unit tests run under
`cargo test`.

## Boot model

`qemu-system-aarch64 -M virt -kernel <elf>` loads the ELF at its link
address (`0x4020_0000`, 2 MiB above the `virt` RAM base) and enters its
entry point with the Linux aarch64 boot-protocol hand-off
(`x0 = DTB`). The `_start` trampoline (`boot.s`):

1. Masks interrupts (`DAIFSet`).
2. If entered at EL2 (a `virtualization=on` board), configures EL1 to
   run AArch64 (`HCR_EL2.RW`), grants EL1/EL0 the physical counter and
   timer (`CNTHCTL_EL2`), zeroes `CNTVOFF_EL2`, and `eret`s to EL1. On
   the default `virt` machine the highest EL is already EL1, so this is
   skipped.
3. Establishes the boot stack, zeroes `.bss`, and tail-calls
   `rustos_arch_aarch64_main(dtb)`, which forwards to the
   binary-supplied `kernel_main`.

The console (`serial.rs`) writes the boot log through the `virt` board's
PL011 UART at `0x0900_0000`, which QEMU routes to `-serial stdio`.

## Result protocol

The `virt` board has no `SiFive` Test device; QEMU verticals report
their result through **ARM semihosting** (`kernel/arch/aarch64::qemu_exit`).
With `-semihosting-config enable=on,target=native`, the guest issues a
`SYS_EXIT` semihosting call (`HLT #0xF000`): a success exit makes QEMU
exit with status `0`, any other status is a failure. This matches
riscv64's zero-is-pass convention (and is the inverse of x86_64's
non-zero `isa-debug-exit`), so the host-side decode is per-arch
(`rustos_qemu::aarch64::outcome_from_status`).

## Stage 3 architecture primitives

Each keeps its pure math host-testable and gates only the
system-register/assembly/MMIO operations to the freestanding target.

- **MMU / page tables** (`paging`). Stage-1, 4 KiB granule, three levels
  (start at L1) covering a 39-bit VA region (`TCR_EL1.T0SZ = 25`) — the
  aarch64 mirror of riscv64's Sv39. `AddressSpace::new_identity_gigapages`
  identity-maps the low GiBs with 1 GiB L1 block descriptors (GiB 0 as
  Device for the UART/GIC MMIO, the rest as privileged-executable Normal
  for the kernel image and stack); `map_4k` adds finer mappings; `switch`
  programs `MAIR_EL1`/`TCR_EL1`/`TTBR0_EL1` and enables `SCTLR_EL1.M`.
- **Context switch** (`context` + `context.s`). `TaskCtx { sp }` plus
  `rustos_arch_aarch64_switch`, saving the AAPCS64 callee-saved registers
  (`x19`–`x28`, `x29`/FP, `x30`/LR) and the first-run argument `x0`.
- **Generic-timer preemption** (`preempt`). The EL1 physical timer
  (`CNTP_*_EL0`) and its GIC PPI (INTID 30): a set-once tick callback,
  `init_local_preempt` (enable the PPI, arm `CNTP_TVAL_EL0`, enable the
  timer), and `on_timer_interrupt` (callback → re-arm).
- **Interrupts** (`exceptions` + `vectors.s`, `gic`). A 16-entry EL1
  vector table (`VBAR_EL1`) routes IRQs to the GICv2 acknowledge →
  timer → end-of-interrupt handshake, an EL0 `svc` (lower-EL synchronous
  exception) to the installed syscall dispatch callback, and any other
  synchronous exception to the installed `fault` handler. `gic` is a
  GICv2 distributor / CPU-interface / SGI driver.
- **Syscall entry** (`syscall_entry`). The `svc` exception class decode
  and the `x8`/`x0`–`x5` → `rustos_abi` `[u64; SYSCALL_MAX_ARGS]`
  marshalling, with a set-once dispatch callback (the same shape the
  x86_64 and riscv64 ports install). The EL0 register frame is now wired
  through: the `vectors.s` trampoline passes the saved-frame base to the
  handler, which on a lower-EL `svc` reads the registers via the
  host-tested `syscall_frame_from_saved`, forwards `(x8, &args)` to the
  dispatch callback, and writes the result back into the saved `x0` slot
  so the `eret` returns it to EL0 (the aarch64 analogue of riscv64's
  `ecall` dispatch). Absent a callback it fails closed. The
  architecture-neutral validation/capability/audit dispatcher lives in
  `kernel/syscall`.

## QEMU verticals

Seven freestanding integration binaries cover the Stage-3 per-sub-stage
checklist (plus the CCOMPAT CC2 syscall round-trip, the Stage W3-B
device-IRQ vertical, the Stage W6 SMP/IPI vertical, and the Stage W7
live-scheduler vertical) on the `virt` board; each links only the arch
port (the live-scheduler vertical also links the
`rustos-kernel-sched-mlfq` policy) and reports its result through the
semihosting finisher. They are enrolled in `cargo xtask test --qemu`.

- `rustos-test-kernel-arch-boot-aarch64` — **boots to init**: the
  trampoline reaches `kernel_main` at EL1 and logs over the PL011 UART.
- `rustos-test-timer-preempt-qemu-aarch64` — **timer interrupt drives
  the scheduler**: arms the EL1 physical timer at 100 Hz and confirms the
  GICv2 IRQ path drives the `preempt` callback ≥ 20 times.
- `rustos-test-irq-qemu-aarch64` — **a device IRQ reaches a Rust
  handler** (Stage W3-B): binds the PL031 RTC's GICv2 SPI (INTID 34) in a
  kernel-neutral `rustos_kernel_irq::IrqTable`, routes that SPI to CPU 0
  through `gic::route_spi` (`GICD_ITARGETSR`), installs a set-once
  device-IRQ dispatcher (`exceptions::set_device_irq_dispatch`) that
  forwards the line to `IrqTable::fire` over a `GicController` bridge,
  and arms the RTC match. When it fires, the GIC delivers the SPI to EL1
  and the dispatcher masks the line + sets the wait flag; the test then
  asserts the GIC enable bit re-reads masked (mask-before-wake,
  `docs/src/security/irq.md`) before the PASS finisher.
- `rustos-test-memory-isolation-qemu-aarch64` — **memory-isolation test
  passes**: a victim and an attacker stage-1 address space disagree on
  one page; switching to the attacker and reading that page raises a
  data abort the `fault` handler confirms.
- `rustos-test-ipi-smp-qemu-aarch64` — **multi-core bring-up + IPI**
  (Stage W6): the boot core starts core 1 through `smp::start_secondary`
  (PSCI `CPU_ON`), waits for it to bring up its GICv2 interface and
  enable the IPI SGI, then delivers a directed IPI through
  `Aarch64Arch::send_ipi` (a GICv2 SGI); PASS once core 1's IRQ path runs
  the IPI callback with core 1's id. Runs with `--cpus 2`.
- `rustos-test-sched-drive-qemu-aarch64` — **the arch primitives drive
  the live scheduler** (Stage W7): the EL1/GICv2 analogue of
  `rustos-test-sched-drive-qemu-riscv64`. With interrupts off it performs
  a real bidirectional `context::switch` round-trip, then builds a real
  `rustos_kernel_sched_mlfq::Scheduler` over `Aarch64Arch` and installs
  the `preempt` generic-timer callback **and** the GICv2 IPI (SGI)
  callback so both drive `Scheduler::on_timer_tick`. It arms the 100 Hz
  generic timer + IPI, spawns and dispatches a batch of tasks through the
  cooperative `step` loop, sends itself a directed IPI, and PASSes once
  the timer IRQ has driven the live scheduler ≥ 20 times and the IPI SGI
  path has driven it at least once. Single CPU.
- `rustos-test-abi-sys-syscall-qemu-aarch64` — **CC2 `svc` round-trip**
  (`plans/CCOMPAT.md`): stands up a minimal EL0 context — identity-maps
  the kernel (EL1), aliases the `lib/abi-sys` `ros_sys_cap_query` stub
  page at a user VA with EL0-executable attributes
  (`paging::el0_code_leaf_attrs`, mapped via `map_4k_with_attrs`) plus an
  EL0 stack (`el0_data_leaf_attrs`), installs the dispatch callback and
  the EL1 vector table, and `eret`s to EL0. The stub's real `svc` then
  traps into the EL1 vector and the callback asserts the kernel-observed
  `(number, args)` are exactly what `ros_sys_cap_query` should have
  marshalled into `x8`/`x0` before the semihosting PASS finisher; any
  mismatch (or the `svc` resuming in EL0) trips a distinct failure
  finisher. This exercises the real `svc` (`lib/abi-sys/src/trap.rs`) and
  the EL0 dispatch wiring together.

## Building and running

```text
# Build a vertical for the freestanding target:
cargo build --locked -p rustos-test-timer-preempt-qemu-aarch64 \
    --target aarch64-unknown-none

# Run it under QEMU (runner exit 0 == PASS):
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/aarch64-unknown-none/debug/rustos-test-timer-preempt-qemu-aarch64 \
    --arch aarch64 --cpus 1 --timeout-secs 60

# Host unit tests for the arch crate (paging / context / preempt /
# syscall / fault / gic / kernel_arch):
cargo test -p rustos-arch-aarch64
```

The host-side argv contract lives in `tools/qemu/src/aarch64.rs`:
`-M virt -cpu cortex-a72 -no-reboot -display none -serial stdio
-semihosting-config enable=on,target=native -m 256M -smp N -kernel <elf>`,
with virtio-mmio block/net devices and `ramfb` attached on demand
(mirroring the riscv64 `virt` backend).

## Platform discovery (hardware tree)

The aarch64 port implements the Arch HAL `PlatformDiscovery` slice
(`AGENTS.md` §17.2 / §18.2) in `kernel/arch/aarch64::platform`, reading
the flattened device tree the `virt` board hands the kernel. The
device-tree *parser* is the shared `lib/fdt` crate (one parser for every
arch, §2.2); `kernel/arch/aarch64::fdt` layers the aarch64-specific
queries on it: the first `/memory` region, the `/psci` `method`
(`hvc`/`smc` — the conduit the Stage W6 secondary-core bring-up calls),
and the generic-timer per-CPU interrupt (PPI) number from
`/timer`. `FdtDiscovery` emits a root node, a `Memory` node carrying the
RAM window, and a `Timer` node carrying its PPI as a capability-gated
(`CAP_IRQ_BIND`) IRQ resource. The reader is host-tested against the
shared DTB fixture and exercised by the port's
`passes_arch_hal_conformance_suite`.

## Per-CPU storage (`TPIDR_EL1`)

The aarch64 port implements the Arch HAL `PerCpu` slice (`AGENTS.md`
§17.2) in `kernel/arch/aarch64::percpu_hal` over the **`TPIDR_EL1`**
system register — the EL1-private thread pointer the kernel uses as its
per-CPU anchor (the EL0 `TPIDR_EL0` belongs to user TLS and is never
touched). `PerCpuStorage::read_self_base` / `write_self_base` are a
single `mrs` / `msr TPIDR_EL1`; the word is opaque (the kernel decides
whether it holds a per-CPU control-block address or a dense `CpuId`). On
the host build there is no `TPIDR_EL1`, so the handle backs the word with
an in-handle cell solely for the round-trip + isolation conformance
verticals (`percpu::conformance`), folded into the port's
`passes_arch_hal_conformance_suite`.

## Interrupt controller (GICv2)

The aarch64 port implements the Arch HAL `IrqController` and
`InterruptEntry` slices (`AGENTS.md` §17.2 / `plans/WIRING.md` Stage W3)
on `kernel/arch/aarch64::gic::GicController`. The GICv2 register logic
was lifted behind a host-testable `GicMmio` seam (mirroring riscv64's
`PlicMmio`, §2.2): a low-level `Gicv2<M>` driver carries the one MMIO
path — enable/disable (`ISENABLER`/`ICENABLER`), priority, `init`,
acknowledge (`IAR`), end-of-interrupt (`EOIR`), and SGI raise — and the
freestanding `init`/`enable_ppi`/`acknowledge`/`end_of_interrupt`/
`send_sgi` free functions are now thin wrappers over a
`Gicv2<VolatileGicMmio>`, so there is no duplicate register logic.
`IrqController::mask` / `unmask` clear / set the distributor enable bit
(mask pairs the write with a `SeqCst` fence for mask-before-wake) and
reject an INTID above `MAX_INTID` with `IrqControlError::OutOfRange`;
`InterruptEntry::claim` / `complete` are the `IAR`/`EOIR` handshake, with
the spurious INTID (`1023`) mapping to `None`. The
`gic_controller_passes_arch_hal_irq_conformance` host test drives both
conformance verticals over a real `GicController` on a mock MMIO (INTID
42 valid, 2000 out of range).

### Device-IRQ delivery (Stage W3-B)

GICv2 shared-peripheral interrupts (SPIs, INTID `>= MIN_SPI_INTID`) reset
to *no* CPU target, so a device interrupt is never delivered until its
`GICD_ITARGETSR` byte names a CPU. `Gicv2::route_spi(intid, cpu_targets)`
(and the freestanding `gic::route_spi` wrapper) writes that byte — the
SPI analogue of the x86_64 IO-APIC redirection-entry destination — and
skips SGIs/PPIs, whose target bytes are read-only and banked per CPU. On
the IRQ path, `exceptions::handle_irq` dispatches the timer PPI to the
scheduler-tick path and forwards **any other** acknowledged INTID to a
set-once device-IRQ dispatcher published through
`exceptions::set_device_irq_dispatch` (the EL1 analogue of riscv64's
`set_trap_dispatch`); the GIC `EOIR` handshake stays in the vector path.
The `route_spi` register arithmetic, the `MIN_SPI_INTID` boundary, and
the fail-closed set-once dispatch slot are host-tested; the
`rustos-test-irq-qemu-aarch64` vertical above proves the full SPI → GIC →
EL1 → dispatcher → `IrqTable::fire` path end-to-end under QEMU.

## SMP secondary-core bring-up (PSCI + GICv2 IPI)

The aarch64 port brings secondary cores up through PSCI (`plans/WIRING.md`
Stage W6), in `kernel/arch/aarch64::smp`:

- `smp::set_secondary_entry` installs a set-once `extern "C" fn(CpuId) -> !`
  that a freshly-started core runs. A set-once callback (rather than a
  mandatory `extern` symbol) keeps secondary bring-up opt-in without a
  Cargo feature, so the single-core boot pipeline and the freestanding
  test bins still link.
- `smp::start_secondary` validates the dense `CpuId` against the
  secondary-stack pool, confirms an entry is installed, then issues a
  PSCI `CPU_ON` (`kernel/arch/aarch64::psci::cpu_on`) through the conduit
  (`hvc`/`smc`) the `fdt` reader discovers, entering the core at the
  `smp.s` trampoline. The trampoline masks interrupts, seeds the core's
  slice of the `.bss` secondary-stack pool (indexed by the dense id PSCI
  passes as the `context_id`), and tail-calls the installed entry. It
  fails closed (`StartCpuError`) on an out-of-range id, a missing entry,
  or a PSCI error rather than assuming the core came up.
- `smp::current_cpu_index` reads the running core's affinity from
  `MPIDR_EL1`; the IRQ path (`exceptions::handle_irq`) forwards it to the
  per-CPU timer slot and the IPI callback (one identity source, §2.2).
  `Aarch64Arch` holds the dense-`CpuId`↔`MPIDR` map (built by
  `Aarch64Arch::with_cpus`); `SchedulerArch::current_cpu` reverse-maps the
  running affinity through it.

A directed IPI is a GICv2 software-generated interrupt (SGI): `send_ipi`
raises INTID 0 on the target CPU through `gic::send_sgi`, and
`exceptions::handle_irq` dispatches an acknowledged SGI (INTID
`< MIN_SPI_INTID`) to `preempt::on_ipi_interrupt` → the IPI callback
installed via `preempt::set_ipi_callback`. This replaces the former
single-CPU self-target best-effort send. The `rustos-test-ipi-smp-qemu-aarch64`
vertical above proves the full start-core → enable-IPI → directed-SGI →
callback path on two emulated cores.

Non-PSCI spin-table boot (e.g. a bare Raspberry Pi 3, whose firmware
parks secondaries on a release address rather than offering PSCI) is a
tracked follow-up: `start_secondary` would gain a spin-table branch
selected from the device tree's `enable-method`. The QEMU `virt` board
and UEFI platforms use PSCI, so the PSCI path is the one exercised today;
the port adds the spin-table path when a spin-table target lands so it is
covered by a real vertical rather than shipped untested (`AGENTS.md`
§2.1 / §2.5).

## Timer programming (`Timer`)

The aarch64 port implements the Arch HAL `Timer` slice (`AGENTS.md`
§17.2 / `plans/WIRING.md` Stage W4) in `kernel/arch/aarch64::timer_hal`
(struct `TimerHal`) over the EL1 physical generic timer wired in
`kernel/arch/aarch64::preempt`. `TimerHal::set_tick_callback` /
`tick_callback` forward to the `preempt` callback static, and
`dispatch_tick` invokes it. The IRQ exception path's
`preempt::on_timer_interrupt` dispatches each generic-timer interrupt
through `TimerHal::dispatch_tick`, so the callback invoke lives in one
place (§2.2); the `CNTP_TVAL_EL0` / `CNTP_CTL_EL0` re-arm and the GIC
PPI enable stay in `preempt` (§2.4). On the host build the handle
forwards to the same static, so the `passes_timer_conformance` host test
runs `timer::conformance::run_all` over a real `TimerHal`. The
`timer_preempt_qemu_aarch64` vertical installs its tick callback through
`TimerHal` and stays green through the HAL.

## Context switch (`ContextSwitch`)

The aarch64 port implements the Arch HAL `ContextSwitch` slice
(`AGENTS.md` §17.2 / `plans/WIRING.md` Stage W5) in
`kernel/arch/aarch64::context_hal` (struct `ContextSwitchHal`) over the
bare-metal task-switch primitive in `kernel/arch/aarch64::context`
(`TaskCtx { sp }` + `context.s`'s `x19`–`x30` save/restore).
`ContextSwitchHal::prepare` seeds a never-run task's first frame and
`switch` performs the EL1 switch. The neutral `TaskContext` and the
port's `TaskCtx` are both a single `#[repr(C)]` `u64`, so the handle
reinterprets the pointer and forwards to `context` (a const-assert pins
the layout equality); the switch invoke lives in one place (§2.2). The
`prepare` contract is host-tested via `context::conformance::run_all`
(`passes_context_switch_conformance`); the switch itself, like
`enter_user`, is proven only on the bare-metal target (the scheduler-
drive vertical), so it carries no host check (§2.1 — no fake primitive).

## MMU / page-table (`AddressSpace`)

The aarch64 port implements the Arch HAL `AddressSpace` slice
(`AGENTS.md` §17.2 / `plans/WIRING.md` Stage W5b-1) on its
`kernel/arch/aarch64::paging::AddressSpace` (the three-level, 4 KiB-granule
stage-1 table programmed into `TTBR0_EL1` with `SCTLR_EL1.M`).
`AddressSpace::map_page` translates the neutral `PageFlags` into a stage-1
leaf-attribute word — W^X by default (`AGENTS.md` §19.2): a `USER | EXEC`
page is mapped read-only EL0-executable (`el0_code_leaf_attrs`), a
`USER | WRITE` page execute-never (`el0_data_leaf_attrs`), a read-only
user page execute-never (`el0_rodata_leaf_attrs`), a kernel page EL1 RW
EL0-XN (`normal_leaf_attrs`), and a `DEVICE` page `device_leaf_attrs` —
then walks the table (reusing `map_4k_with_attrs`, one walk, §2.2), failing
closed (`Misaligned`/`AlreadyMapped`/`PoolExhausted`/`InvalidFlags`).
`root_phys` returns the L1 root and `activate` forwards to the gated
`switch` (the `TTBR0_EL1`/`SCTLR_EL1.M` enable). Because the walk recovers
intermediate tables through the identity map (phys == virt), the whole
`map_page` path is host-runnable: `passes_mmu_conformance` drives
`mmu::conformance::run_all` over a real `AddressSpace`, and a companion
host test asserts the W^X leaf-attribute translation. The `activate`
register write itself is proven by `memory_isolation_qemu_aarch64`, which
now builds its victim/attacker spaces through this trait.

## Heterogeneous (`big.LITTLE`) core classification

The aarch64 port overrides the Arch HAL `SchedulerArch::core_class`
slice (`AGENTS.md` §17.2 / `plans/WIRING.md` Stage W10) so the scheduler
can place background work on the efficiency cores of an asymmetric part.
Unlike x86_64, where each core reports its class through a per-core CPUID
leaf, Arm advertises per-core capacity in the device tree: each
`/cpus/cpu@*` node carries an optional `capacity-dmips-mhz` rating.

`Aarch64Arch::classify_from_fdt` enumerates those nodes through the
shared reader's `rustos_fdt::Fdt::each_cpu` (one device-tree parser,
§2.2), maps each node's `reg` (its `MPIDR_EL1` affinity) to a dense
`CpuId` through the same affinity map `current_cpu`/`send_ipi` use, and
classifies the collected ratings with the pure
`kernel/arch/aarch64::hetcore::classify_by_capacity`: the highest rating
present is the performance tier, and any core rated strictly below it is
an efficiency core. A homogeneous machine — every rating equal, no
ratings at all, or a malformed tree — leaves every core a performance
core, the safe Arch HAL default; a core with no advertised rating is
never guessed down. The classified table is read back through the
`core_class` override, which returns the performance default for an
out-of-range `CpuId` (totality, never a panic).

The classifier is pure and host-tested (`hetcore`'s unit tests), and the
device-tree read is host-tested against the shared `rustos_fdt::fixture`
`big.LITTLE` builder, so `classify_from_fdt` is proven end-to-end on the
host (`classify_from_fdt_reports_big_little_cores`); the shared HAL
conformance vertical asserts `core_class` totality on every port.
riscv64 (homogeneous) keeps the default, so adding a heterogeneous
RISC-V part is a `core_class` override there, not a change here.

## virtio-MMIO device verticals (Stage W11-A)

The `virt` board's virtio-MMIO bus is driven end-to-end by two QEMU
verticals, the EL1/GICv2 analogue of the riscv64 ones:
`tests/integration/virtio_blk_mmio_aarch64` (read sector 0 and verify the
host-planted pattern, then write and read back sector 1) and
`tests/integration/virtio_net_mmio_aarch64` (ARP-resolve the QEMU
user-mode gateway `10.0.2.2` from guest `10.0.2.15`, then ICMP echo).
Both are enrolled in `tools/xtask/src/commands/qemu_tests.rs` and report
through the `SYS_EXIT` semihosting finisher.

The device-agnostic bring-up lives in the shared
`tests/integration/virtio_qemu_support` crate's `imp_mmio_aarch64` module
(behind `cfg(itest_aarch64)`), and the device round-trip *tails* are the
same shared code the riscv64 and x86_64 verticals run (`AGENTS.md` §2.2).
The module owns only what is unique to the EL1 bring-up:

- **FP/SIMD enable.** The `virt` board enters EL1 with
  `CPACR_EL1.FPEN` trapping Advanced-SIMD/FP; the compiler emits NEON
  register moves for the struct copies in the driver/DMA stack, so the
  scenario sets `FPEN = 0b11` first (a trapped access otherwise faults
  with `ESR_EL1` EC `0x07`). riscv64 gets the equivalent FP enable from
  its boot pipeline.
- **Stage-1 MMU.** It brings up a 2 GiB identity map through
  `paging::AddressSpace::new_identity_gigapages` (GiB 0 Device memory for
  the GIC/PL011/virtio-MMIO apertures, RAM Normal-cacheable). The MMU-off
  reset state types every access as Device, where the `LDXR`/`STXR`
  atomics the driver/DMA/sync stack relies on abort; mapping RAM as Normal
  memory is the precondition for the rest of the bring-up.
- **IRQ path.** It walks the device tree for the provisioned slot's GICv2
  SPI, wires the EL1 device-IRQ dispatch (`exceptions::set_device_irq_dispatch`)
  to a `kernel/irq` `IrqTable` over a `GicController` bridge (the bridge
  lives in the test crate, since §17.4 forbids the arch crate depending on
  `kernel/irq`), and parks the boot CPU on a race-free DAIF-masked `wfi`.

QEMU's `-kernel <ELF>` aarch64 path treats the image as bare firmware and
passes no DTB pointer (`x0 == 0`), unlike the riscv64 OpenSBI `a1`
hand-off. Each vertical therefore embeds the canonical `virt` DTB, dumped
at build time by `qemu-system-aarch64 ... dumpdtb` (gated to the
aarch64-none target), and hands those bytes to the scenario; the
virtio-MMIO transport bases and SPIs in that blob are the stable
`virt`-board layout, independent of which transport slot the backing
device lands on. The display and input verticals (Stage W11-B) reuse this
bring-up.
