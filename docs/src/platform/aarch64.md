# aarch64

RustOS targets `aarch64-unknown-none` as a Tier-1 platform. Stage 3b
delivers the QEMU `virt`-board boot and Arch-HAL primitives for the
64-bit Arm port: an EL1 boot trampoline, a PL011 UART console, the
`Aarch64Arch` implementation of the Arch HAL, the EL1 exception vector
table, a GICv2 driver, generic-timer preemption, the stage-1 MMU
primitives, the `svc` syscall-entry marshalling, and the ARM semihosting
test finisher. This page documents the boot model, the result protocol,
the arch primitives, and the QEMU argv contract.

## Raspberry Pi 4 (BCM2711)

This section is the authoritative *facts of record* for the Raspberry Pi 4 /
Pi 400 (BCM2711, quad Cortex-A72, GIC-400). It pins the numbers the
`plans/PI.md` Pi-4 bring-up depends on so every later stage cites one source
rather than re-deriving the MMIO map or boot protocol (`AGENTS.md` §13,
§15.7 — no guessing). The Pi 3 (BCM2837, no GIC) and Pi 5 (BCM2712, RP1
southbridge) are out of scope here; they reuse this work as later board ports.

Per `plans/PI.md` §0.2, the `virt` board and the Pi 4 are two *boards of the
same `aarch64` architecture*: every base below is consumed as **runtime
device-tree data** discovered by `kernel/arch/aarch64::platform::FdtDiscovery`
into `rustos_abi::hwtree`, never a `cfg(board = …)` fork (`AGENTS.md` §17.2 /
§2.2). The numbers are recorded here only as the facts the discovery path must
yield, and as the input to the one legitimate per-board artefact — the boot
stub, linker script, and load address (the `AGENTS.md` §1 boot-stub carve-out).

### MMIO map (ARM-physical addresses)

The BCM2711 maps its "low peripheral" region (VideoCore bus alias
`0x7E00_0000`) to ARM physical base **`0xFE00_0000`**. The peripherals this
plan touches sit at the following offsets from that base:

| Peripheral | Offset from `0xFE00_0000` | ARM-physical address |
| --- | --- | --- |
| PL011 UART (`uart0`, `arm,pl011`) | `+0x20_1000` | `0xFE20_1000` |
| AUX mini-UART (`uart1`, `brcm,bcm2835-aux-uart`) | `+0x21_5040` | `0xFE21_5040` |
| VideoCore mailbox | `+0x00_B880` | `0xFE00_B880` |
| EMMC2 SD host | `+0x34_0000` | `0xFE34_0000` |

The AUX block base is `0xFE21_5000` (the `brcm,bcm2835-aux` peripheral); the
mini-UART register window begins at `+0x40` (`0xFE21_5040`), behind the AUX
enable register at `+0x04`. The mini-UART register layout differs from the
PL011 (a narrower, 7-bit-addressed 16550-derived block), which is why P2 adds
it as a second console backend behind one `rustos_log::Sink` seam, selected by
the device-tree `compatible` string.

### Interrupt controller (GIC-400)

The BCM2711 carries an Arm **GIC-400** (a GICv2 implementation), distinct from
the legacy BCM2836/2837 local+ARMC interrupt controllers. Its bases are:

| GIC-400 block | ARM-physical address |
| --- | --- |
| Distributor (`GICD`) | `0xFF84_1000` |
| CPU interface (`GICC`) | `0xFF84_2000` |

The GICv2 register layout already implemented by
`kernel/arch/aarch64::gic` matches GIC-400 unchanged; only the bases move, and
they are threaded from `FdtDiscovery` (P3) rather than the `virt` constants.

### Boot protocol

The Pi firmware (`start4.elf`, with `fixup4.dat`) loads the kernel image
`kernel8.img` to physical **`0x8_0000`** (the AArch64 64-bit load address) and
enters it in **AArch64 EL2** with the Linux aarch64 hand-off convention
(`x0` = physical address of the firmware-supplied DTB; `x1`/`x2`/`x3` zero).
On current firmware all four cores are released to the kernel entry unless an
`armstub8.bin` spin-table stub is supplied; the boot stub must therefore park
secondaries (`MPIDR_EL1` affinity ≠ 0) until SMP bring-up wants them (P1/P5).

`config.txt` knobs the bring-up relies on:

| Key | Value | Effect |
| --- | --- | --- |
| `arm_64bit` | `1` | boot the ARM cores in AArch64 |
| `kernel` | `kernel8.img` | the image the firmware loads at `0x8_0000` |
| `enable_uart` | `1` | hold the core clock so the PL011/mini-UART baud is stable; route the debug UART |
| `armstub` | `armstub8.bin` | optional PSCI-providing secondary-core stub (enables the `smc`-conduit PSCI `CPU_ON` path of P5) |

The PSCI conduit on the Pi is **`smc`** (via `armstub8.bin`), versus `hvc` on
the QEMU `virt` board; it is discovered through `fdt::psci_method`, never
assumed (P5).

### RAM layout

The BCM2711 places usable DRAM at physical base **`0x0`** (the QEMU `virt`
board uses `0x4000_0000`). The four SKUs and the high-memory window:

| SKU | Low window | High window (`>3 GiB`) |
| --- | --- | --- |
| 1 GiB | `0x0`–`0x3FFF_FFFF` | — |
| 2 GiB | `0x0`–`0x7FFF_FFFF` | — |
| 4 GiB | `0x0`–`0xBFFF_FFFF` | — |
| 8 GiB | `0x0`–`0xBFFF_FFFF` (low 3 GiB) | `0x1_0000_0000`–`0x2_3FFF_FFFF` |

The SoC's 35-bit address space aliases the peripheral and high-RAM windows;
the firmware DTB's `/memory` node(s) report the SKU's actual extents, which
`FdtDiscovery::first_memory_region` reads — the allocator must not assume the
`virt` `0x4000_0000` base. Since **P3** the boot path reads that window from
the DTB; since **P6c-1** it translates the window (plus the linker
`__kernel_end`) into the canonical two-region `BootMemoryMap` the live
allocator hand-off consumes — `[ram_base, __kernel_end)` reserved, the
page-aligned remainder usable — and logs the resulting split
(`mem_map_built` / `mem_map_status` / `usable_bytes_hex` /
`reserved_bytes_hex`), failing closed to a status string (never a panic,
`AGENTS.md` §2.9) on an absent or malformed window. The arithmetic is the
host-tested `rustos_kernel::mem_map` module (the riscv64 boot pipeline's
`build_memory_map` analogue). Handing that map to
`kernel_core::kernel_main` is P6c-2 — it needs the MMU enabled first, since
the allocator/scheduler atomics are UNPREDICTABLE on the MMU-off
Device-memory the boot CPU runs on (a hard-coded map would violate
`AGENTS.md` §18.5).

### Production kernel image (P1)

The production aarch64 kernel is the `rustos-kernel` binary built for
`aarch64-unknown-none`. Its boot artefacts are the one legitimate per-board
fork (`AGENTS.md` §1 boot-stub carve-out; `plans/PI.md` §0.2):

- **Linker script** `kernel/arch/aarch64/link/aarch64-rpi4.ld` places the
  image at the firmware load address `0x8_0000`. It is identical to
  `aarch64-virt.ld` (used by the QEMU `virt` per-test bins) save for the
  origin address. `kernel/rustos-kernel/build.rs` selects it for the
  `aarch64-unknown-none` target; the pure target→linker/`kernel_isa`
  selection logic lives in the host-unit-tested `src/build_support.rs`, so
  the crate body never names `target_arch` (cfg-check clean).
- **Boot stub** `boot.s` is board-independent. Before touching the shared
  boot stack it parks every CPU whose `MPIDR_EL1` affinity is non-zero in a
  `wfe` loop — correct on the Pi (all four cores released at reset) and a
  no-op on `virt` (secondaries held in firmware until PSCI). The boot CPU
  drops EL2→EL1 (if entered at EL2, as the Pi firmware does) and tail-calls
  `kernel_main(dtb)`.
- **`kernel_main(dtb)`** enables FP/SIMD via `rustos_arch_aarch64::enable_fp_el1`,
  constructs `Aarch64Arch` (the single `AGENTS.md` §17.1/§17.2 concrete-arch
  selection point for the image), records a boot audit line over the
  console, and parks fail-closed.

Since P2 the console base is **device-tree-discovered**: `kernel_main`
calls `rustos_arch_aarch64::console::configure_from_fdt` on the `x0` DTB
before its first log line, so the console points at whatever UART the
firmware tree describes (see [Board-discovered console](#board-discovered-console)).
Since **P3** it also points the GICv2 driver at the discovered GICD/GICC
bases (`gic::configure_from_fdt`) and reads the `/memory` window, logging
`gic_discovered` / `ram_discovered` (see
[Board-discovered interrupt controller](#board-discovered-interrupt-controller)).
Since **P5** it discovers the PSCI conduit (`fdt::psci_method`) and
installs it on the handle (`with_psci_method`), logging
`psci_conduit_discovered`, so SMP bring-up issues `CPU_ON` over the
conduit the board declares (`hvc` on `virt`, `smc` on the Pi) rather than
an assumed one (see
[SMP secondary-core bring-up](#smp-secondary-core-bring-up-psci--gicv2-ipi)).
Since **P6c-1** it builds the canonical `BootMemoryMap` from the discovered
`/memory` window and records its usable/reserved split (`mem_map_built` /
`mem_map_status` / `usable_bytes_hex` / `reserved_bytes_hex`); see
[RAM layout](#ram-layout). The discovery-fed `kernel_core::kernel_main`
hand-off over that map (which first enables the MMU so the allocator's
atomics run on Normal memory) is staged to P6c-2; a hard-coded map would
violate `AGENTS.md` §18.5.

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
entry point. The `_start` trampoline (`boot.s`) follows the Linux aarch64
boot protocol's `x0 = DTB` register convention, which real firmware (and
the Pi GPU firmware) populates; note that QEMU's `-kernel <ELF>` path
itself passes `x0 = 0` (it treats the image as bare firmware), so the
verticals that need the board tree embed it at build time rather than
reading the pointer (see [Board-discovered console](#board-discovered-console)).
The trampoline:

1. Masks interrupts (`DAIFSet`).
2. If entered at EL2 (a `virtualization=on` board), configures EL1 to
   run AArch64 (`HCR_EL2.RW`), grants EL1/EL0 the physical counter and
   timer (`CNTHCTL_EL2`), zeroes `CNTVOFF_EL2`, and `eret`s to EL1. On
   the default `virt` machine the highest EL is already EL1, so this is
   skipped.
3. Establishes the boot stack, zeroes `.bss`, and tail-calls
   `rustos_arch_aarch64_main(dtb)`, which forwards to the
   binary-supplied `kernel_main`.

The console (`serial.rs`) writes the boot log through whatever UART the
`console` module currently points at; before any discovery runs that is
the `virt` board's PL011 at `0x0900_0000`, which QEMU routes to
`-serial stdio`.

## Board-discovered console

The console MMIO base and register model are **discovered from the
firmware device tree**, not hard-wired (`plans/PI.md` P2). The
host-testable `console` module holds the active `(base, model)` as an
atomic pair (the pre-discovery default is the `virt` PL011 base), and the
freestanding `serial` sink reads it on every transmitted byte.
`console::find_console` / `configure_from_fdt` walk the shared `lib/fdt`
reader for the first node whose `compatible` names a model the port
speaks, preferring the PrimeCell **PL011** (`arm,pl011`) over the BCM2835
AUX **mini-UART** (`brcm,bcm2835-aux-uart`). The two models are one
console abstraction with two register backends — distinct data/status
register offsets and opposite-sense transmit-ready bits — not duplication
(`AGENTS.md` §2.2). `platform::FdtDiscovery` also emits a `serial`-class
`HwNode` carrying the discovered `compatible` bind key and the UART `reg`
as a capability-gated MMIO resource.

The runtime walk is safe with the MMU still off: the `lib/fdt` reader
accesses the blob byte-by-byte, so it takes no multi-byte Device-memory
load that would fault without exception vectors (`plans/PI.md` W17).

**QEMU caveat (honest emulation gap, `AGENTS.md` §2.1).** QEMU's `raspi*`
machine models do **not** emulate the Raspberry Pi GPU-firmware DTB
hand-off: they enter an ELF `-kernel` with `x0 = 0` (GDB-verified on
`raspi3b`), and QEMU 8.2.2 ships no `raspi4b`. The `virt` board *does*
hand the kernel a generated tree (with a real `arm,pl011` node), so the
runtime discover→configure→print path is CI-proven on `virt` against a
genuine firmware tree; the Pi's specific console base and the mini-UART
register layout are covered by host unit tests against the `rustos_fdt`
`raspi_like_arm` fixture and are on-metal acceptance items for the Arc C
peripheral stages.

### Console input backing (`console_read`)

The same `console` model backs the **input** half (`plans/PI.md` P6e-2):
`ConsoleModel::rx_ready` decodes the model's receive-status bit (the
PL011's `UARTFR.RXFE` is *set* when the receive FIFO is empty; the
mini-UART's `AUX_MU_LSR_REG` bit 0 is *set* when data is ready), reusing
the same status and data register offsets as the transmit path since they
coincide on both models. `serial::read_console_bytes` drains whatever
input is immediately available into the caller's buffer and stops at the
first byte that is not yet present — it **never busy-waits** for input
(`AGENTS.md` §2.1), so a read with no pending byte is a valid zero-length
short read. `boot_aarch64` installs this through the same zero-sized
`UartConsole` device (it implements both `ConsoleWrite` and `ConsoleRead`)
via `BootInfo::with_console_read`, so the `console_read` syscall reads the
discovered UART.

This is the bootstrap stream **backing** the spawner attaches to fd 0
(`AGENTS.md` §20); it is not a program-facing interface. The receive-bit
decoders are host-unit-tested; real RX over `virt`/Pi silicon is exercised
once the standard-stream layer binds fd 0 to it (`plans/PI.md` P6e-3a).

## Board-discovered interrupt controller

The GICv2 distributor (`GICD`) and CPU-interface (`GICC`) MMIO bases are
**discovered from the firmware device tree**, not hard-wired (`plans/PI.md`
P3). The host-testable `gic` module holds the active `(gicd, gicc)` pair as
an atomic (the pre-discovery default is the `virt` GICv2 base
`0x0800_0000` / `0x0801_0000`), and the freestanding `VolatileGicMmio`
accessor reads it on every register access — so a single GICv2 driver
drives both the `virt` board and the Pi 4's **GIC-400** (`arm,gic-400`,
`0xFF84_1000` / `0xFF84_2000`) with no `cfg(board)` fork. GIC-400 *is* a
GICv2, so only the bases move (`AGENTS.md` §2.2). `gic::find_gic` /
`configure_from_fdt` walk the shared `lib/fdt` reader for the first node
whose `compatible` names a GICv2-class controller and read its `reg`
(region 0 = distributor, region 1 = CPU interface); an unrecognised or
absent controller leaves the fail-safe default in place (`AGENTS.md`
§2.9). `platform::FdtDiscovery` emits an `InterruptController` `HwNode`
carrying the discovered `compatible` bind key and both register windows as
capability-gated MMIO resources.

The runtime walk is MMU-off-safe for the same byte-wise reason as the
console (`plans/PI.md` W17), and is CI-proven on `virt`: the
`rustos-test-ipi-smp-qemu-aarch64` vertical **poisons** the GIC base, then
discovers it from the embedded `virt` tree before `gic::init`, so the
delivered IPI exercises the *discovered* base. The Pi 4's specific GIC-400
bases are covered by host unit tests against the `raspi_like_arm` fixture
and are an on-metal acceptance item (no `raspi4b` in QEMU — the same gap
as the console).

## Board-discovered timer frequency

The generic-timer counter rate that sizes the preemption interval is a
**discovered board fact**, not the raw `CNTFRQ_EL0` register alone
(`plans/PI.md` P4). `fdt::timer_clock_frequency` reads the `/timer`
node's optional `clock-frequency` override (the standard `arm,armv?-timer`
binding the firmware carries when `CNTFRQ_EL0` is left mis-programmed),
and the pure, host-tested `fdt::effective_timer_hz` prefers it when
present and non-zero, otherwise falls back to `CNTFRQ_EL0` (a zero
override is treated as absent — never a 0 Hz timer, `AGENTS.md` §2.9).
The freestanding `kernel_arch::timer_frequency_hz(&fdt)` composes the
two, and `boot_aarch64` seeds the `Aarch64Arch` monotonic clock and the
live-timer interval from it, logging `timer_hz_from_tree`. So the QEMU
`virt` board's host-derived rate and the Raspberry Pi 4's 54 MHz crystal
both flow through one path with no `cfg(board)` fork (`AGENTS.md`
§17.2 / §2.2).

The match uses the shared `Fdt::nodes` walk, early-returning at the
first `arm,armv8-timer` node and reading only that node's properties —
the same byte-safe traversal `gic::configure_from_fdt` uses, **not** the
whole-tree `Fdt::property`/`walk` scan (which the compiler can widen into
multi-byte loads that fault under a vertical's MMU-off boot). The `virt`
tree omits `clock-frequency`, so the CI runtime path exercises the
register fallback while the override branch is host-unit-tested; honouring
the Pi's real crystal is an on-metal acceptance item (no `raspi4b` in
QEMU — the same gap as the console / GIC).

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
  timer), and `on_timer_interrupt` (callback → re-arm). The interval is
  sized from the **discovered** counter rate
  (`kernel_arch::timer_frequency_hz`, PI Stage P4 — see
  [Board-discovered timer frequency](#board-discovered-timer-frequency)),
  not a hard-wired frequency.
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

Eight freestanding integration binaries cover the Stage-3 per-sub-stage
checklist (plus the PI Stage P2 console-discovery vertical, the CCOMPAT
CC2 syscall round-trip, the Stage W3-B device-IRQ vertical, the Stage W6
SMP/IPI vertical, and the Stage W7 live-scheduler vertical) on the `virt`
board; each links only the arch port (the live-scheduler vertical also
links the `rustos-kernel-sched-mlfq` policy) and reports its result
through the semihosting finisher. They are enrolled in
`cargo xtask test --qemu`.

- `rustos-test-kernel-arch-boot-aarch64` — **boots the production
  pipeline to `BootCompleted`** (PI Stage P6c-2): drives the real
  `rustos_kernel::boot_aarch64::boot`, which enables the stage-1 identity
  MMU (512×1 GiB gigapages over a static boot `PageTablePool`, then
  `switch`) + EL1 vectors, discovers the board from the embedded `virt`
  device tree, builds the `BootMemoryMap`, installs the discovered-UART
  `console_write` device + the `svc` dispatch callback, and hands a
  validated `BootInfo` to `kernel_core::kernel_main`; the audit sink
  reports PASS on `AuditEvent::BootCompleted` — the aarch64 analogue of
  the x86_64 / riscv64 boot verticals.
- `rustos-test-uart-console-qemu-aarch64` — **the console base is
  discovered, not hard-wired** (PI Stage P2): poisons the console base,
  then proves `console::configure_from_fdt` overwrites it with the base
  read from the board's embedded device tree and that writes reach that
  base (it prints two lines over the *discovered* console before the PASS
  finisher).
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
  (Stage W6) **over a discovered GIC base** (PI Stage P3): the boot core
  first poisons the GICv2 base and rediscovers it from the embedded
  `virt` device tree (`gic::configure_from_fdt`), then starts core 1
  through `smp::start_secondary` (PSCI `CPU_ON`), waits for it to bring up
  its GICv2 interface and enable the IPI SGI, then delivers a directed IPI
  through `Aarch64Arch::send_ipi` (a GICv2 SGI); PASS once core 1's IRQ
  path runs the IPI callback with core 1's id — so the IPI is delivered
  over the *discovered* base. Runs with `--cpus 2`.
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
  path has driven it at least once. **Over discovered values** (PI Stage
  P4): the tick interval is sized from `kernel_arch::timer_frequency_hz`
  read from the embedded `virt` DTB, and the GICv2 base is poisoned then
  rediscovered (`gic::configure_from_fdt`) before `gic::init`, so the
  ticks + IPI run over the *discovered* base and rate, not the
  pre-discovery defaults. Single CPU.
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
and the generic-timer per-CPU interrupt (PPI) number from `/timer` (plus
that node's optional `clock-frequency` counter-rate override, PI Stage
P4). `FdtDiscovery` emits a root node, a `Memory` node carrying the
RAM window, a `Timer` node carrying its PPI as a capability-gated
(`CAP_IRQ_BIND`) IRQ resource, (PI Stage P3) an `InterruptController`
node carrying the GICv2's `compatible` bind key and its GICD/GICC
register windows as MMIO resources, and (PI Stage P2) a `Serial` node
carrying the discovered console UART's `compatible` bind key and its
`reg` as an MMIO resource. The reader is host-tested against the shared
DTB fixtures (including the `raspi_like_arm` Pi-shaped tree, which now
carries a GIC-400 node) and exercised by the port's
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
`VolatileGicMmio` reads the **discovered** GICD/GICC bases
(`gic::current`) on every access, so the same driver serves the `virt`
GICv2 and the Pi 4's GIC-400 (see
[Board-discovered interrupt controller](#board-discovered-interrupt-controller)).
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

Secondary bring-up is reached through the Arch HAL `SecondaryBringup`
slice (`rustos_arch_api::smp`, `plans/WIRING.md` Stage W14):
`Aarch64Arch::start_secondary(cpu)` resolves the dense `CpuId` to its
`MPIDR_EL1` affinity through the handle's map, looks up the PSCI conduit
installed via `Aarch64Arch::with_psci_method` (a missing conduit or an
unmapped id fails closed with `SmpError::NotReady` / `InvalidCpu`), and
delegates to `smp::start_secondary` above. The host
`passes_secondary_bringup_conformance` test runs `smp::conformance::run_all`
over a real handle; the real PSCI `CPU_ON` is proven by the two-core QEMU
verticals, which start their secondary core through this HAL trait —
`cross_cpu_tlb_shootdown_qemu_aarch64` and, since `plans/WIRING.md` Stage
W15, `ipi_smp_qemu_aarch64` — rather than calling the port-private
`smp::start_secondary` directly.

Since `plans/PI.md` **P5** the conduit `with_psci_method` installs is a
*discovered* board fact, not a constant. The production `boot_aarch64`
path reads `/psci` `method` from the `x0` DTB (`fdt::psci_method`) and
installs it on the handle, logging `psci_conduit_discovered`; a tree with
no PSCI node leaves the conduit unset so bring-up fails closed
(`SmpError::NotReady`) rather than assuming one. `fdt::psci_method`
matches the `/psci` node through the shared `Fdt::nodes` early-return
walk (the same byte-safe traversal `gic::configure_from_fdt` and
`fdt::timer_clock_frequency` use, §2.2) — not the whole-tree
`Fdt::property` scan, which faults under the verticals' MMU-off boot once
the compiler widens the byte reads. The `ipi_smp_qemu_aarch64` vertical
proves the discovered conduit drives bring-up: it reads the conduit from
the embedded `virt` tree (asserting it is the board's `hvc`) and starts
the secondary over *that* value, fail-closed if the tree declares none.
The Pi's `smc` conduit (via `armstub8.bin`) flows through the identical
path and is an on-metal acceptance item (no `-M raspi4b` in QEMU).

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

## Cross-CPU TLB shootdown (`CrossCpuTlbShootdown`)

The aarch64 port implements the Arch HAL `CrossCpuTlbShootdown` slice
(`rustos_arch_api::xtlb`, `plans/WIRING.md` Stage W13) on `Aarch64Arch`.
It needs no IPI: `shootdown_page` issues the *inner-shareable broadcast*
`tlbi vaae1is` + `dsb ish`/`isb`, which the hardware propagates to every
PE in the inner-shareable domain. That broadcast is the *same* instruction
the local `TlbShootdown::flush_page` already issues, so both funnel
through one shared `paging::invalidate_page_inner_shareable` helper — the
"local" and "cross-CPU" shootdowns are literally the same operation on
aarch64 (`AGENTS.md` §2.2), and the `dsb ish`/`isb` provide the ordering
the cross-CPU contract requires.

`tests/integration/cross_cpu_tlb_shootdown_qemu_aarch64` proves it on a
real two-core `virt` board: the boot core starts core 1 (PSCI `CPU_ON`),
then drives `Aarch64Arch::shootdown_page`; reaching the PASS finisher
proves the broadcast executes across a multi-PE domain without faulting.
Enrolled in `tools/xtask/src/commands/qemu_tests.rs` (`cpus: 2`, 60 s).

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
The module owns only what is unique to the EL1 bring-up. The FP-enable
and MMU steps are shared as one public helper,
`bring_up_el1_identity_mmu`, reused by both the virtio scenario and the
display vertical (`AGENTS.md` §2.2):

- **FP/SIMD enable.** The `virt` board enters EL1 with
  `CPACR_EL1.FPEN` trapping Advanced-SIMD/FP; the compiler emits NEON
  register moves for the struct copies in the driver/DMA stack, so the
  helper sets `FPEN = 0b11` first (a trapped access otherwise faults
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
hand-off. Each vertical therefore embeds the canonical `virt` DTB and
hands those bytes to the scenario; the virtio-MMIO transport bases and
SPIs in that blob are the stable `virt`-board layout, independent of
which transport slot the backing device lands on.

The dump lives in one place: `rustos_itest_harness::dump_aarch64_virt_dtb`
(gated to the aarch64-none target) shells out to `qemu-system-aarch64 ...
dumpdtb` so every aarch64 vertical reuses one helper rather than copying
the invocation (`AGENTS.md` §2.2). `dumpdtb` pads the blob out to the
machine's 1 MiB device-tree region, so the helper trims it to the extent
its FDT header describes (`trim_fdt_to_extent`, rewriting `totalsize`)
before it is embedded — the few-KiB meaningful tree, not ~1 MiB of zero
padding bloating every image. The trimmed blob is still a valid FDT
(`rustos_fdt::Fdt::new` validates against the buffer length, not
`totalsize`), proven by a round-trip unit test over the shared
`rustos_fdt::fixture` builder and by the device verticals parsing it at
runtime.

## Display vertical (Stage W11-B)

`tests/integration/framebuffer_display_qemu_aarch64`
(`rustos-test-framebuffer-display-qemu-aarch64`, enrolled in
`tools/xtask/src/commands/qemu_tests.rs` with `ramfb: true`) drives the
framebuffer display driver end-to-end on the `virt` board — the EL1/GICv2
+ `ramfb` analogue of the riscv64 framebuffer-display vertical. It reuses
the shared `bring_up_el1_identity_mmu` helper above and the **same**
shared `fw_cfg` MMIO transport (`rustos-itest-fwcfg`'s `MmioDma`) the
riscv64 vertical uses — the two `virt` boards expose `fw_cfg` identically,
so there is one transport, not two (`AGENTS.md` §2.2). The vertical
programs QEMU's `ramfb` over `fw_cfg` so a static guest-RAM surface
becomes a real scan-out framebuffer, assembles the geometry as a
`FramebufferConfig`, loads the signed framebuffer `.rxe` through
`rustos_drvhost::Host`, and drives `load → use → unload → reload`,
mapping the surface through the capability-gated `KernelMmioMapper` and
reading the presented pixels back through an independent window. It
embeds the canonical `virt` DTB (build-time `dumpdtb`) to discover the
`fw_cfg` base, since QEMU's aarch64 `-kernel <ELF>` path passes no DTB
pointer.

## Input vertical (Stage W11-B)

`tests/integration/input_virtio_mmio_qemu_aarch64`
(`rustos-test-input-virtio-mmio-qemu-aarch64`, enrolled in
`tools/xtask/src/commands/qemu_tests.rs` with `keyboard: Some(..)`)
drives the virtio-input driver end-to-end on the `virt` board — the
virtio-input analogue of the x86 PS/2 vertical, completing the `input`
row of the QEMU matrix for aarch64. It reuses the shared
`bring_up_el1_identity_mmu` helper, builds the virtio-MMIO transport from
the embedded `virt` DTB, arms the device's GICv2 SPI, loads the signed
virtio-input `.rxe` through `rustos_drvhost::Host`, and drives
`load → use → unload → reload`.

"Use" is a **real injected key**, the device-side analogue of the PS/2
vertical's `0xD2` output-buffer injection. A `no_std`, non-interactive
guest cannot type at itself, and virtio-input is strictly
device→driver, so the key must originate host-side: the QEMU runner
(`tools/qemu`) attaches a `virtio-keyboard-device`
(`Spec::with_virtio_keyboard`), drains the serial console on a
background thread, and — once the guest logs its event-queue-armed
readiness marker — sends `sendkey` through a QEMU monitor on a private
unix socket. The runner holds that monitor connection open until the run
ends, because a readline monitor discards a command if the peer
disconnects before it is processed. The injected key raises the eventq
SPI, the guest's IRQ path wakes, and the driver decodes the press and —
after reload — the matching release. Two driver-side facts make this
work: the driver pre-posts a *pool* of eventq buffers (QEMU needs a free
buffer for both the `EV_KEY` and its trailing `EV_SYN`), and it
negotiates `VIRTIO_F_VERSION_1`.
