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
enters it in **AArch64 EL2** with the aarch64 boot hand-off convention
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
| `dtoverlay` | `disable-bt` | detach Bluetooth from the PL011 so `UART0` is the primary UART on the GPIO 14/15 header |
| `init_uart_clock` | `48000000` | pin the PL011 reference clock to the 48 MHz the kernel's baud-divisor arithmetic assumes (`uart_init::UART_CLOCK_HZ`) |
| `init_uart_baud` | `57600` (debug) / `9600` (installer) | profile-keyed (`rustos_mkimage::console_baud_for`) so the firmware's own early output matches the kernel's line setting — the kernel then programs the PL011 itself to the same rate, 8 data bits, no parity, 1 stop bit (`uart_init::CONSOLE_BAUD`, gated to the same split). The debug image runs the line faster to drain its verbose boot log; logging is best-effort and never blocks on the UART |
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
the firmware DTB's `/memory` node(s) report the SKU's actual extents — the
allocator must not assume the `virt` `0x4000_0000` base. The boot path reads
**every** declared range (`Fdt::each_memory_region`: every `reg` pair of
every `/memory` node — an 8 GiB Pi 4 declares windows below the MMIO hole,
between 1 GiB and 4 GiB, and above 4 GiB; reading only the first
under-reported it as ~1 GiB), clips the windows out of Device-typed
gigapages (the identity map types memory at 1 GiB granularity and Device
wins for a shared gigapage, so RAM sharing the UART/GIC/PCIe gigapage would
be mapped Device — those bytes are dropped fail-closed until 2 MiB-granular
identity typing lands, `plans/APPS.md` I4), widens the RAM gigapage mask
and the live identity map to cover them, and translates them (plus the
linker `__kernel_end`) into the canonical multi-region `BootMemoryMap` the
live allocator hand-off consumes — `[window base, __kernel_end)` reserved
in the kernel's window, every other window wholly usable — and logs the
resulting split (`mem_map_built` / `mem_map_status` / `usable_bytes_hex` /
`reserved_bytes_hex`), failing closed to a status string (never a panic,
`AGENTS.md` §2.9) on an absent or malformed discovery. The arithmetic is
the host-tested `rustos_kernel::mem_map` module (the analogue of the
riscv64 boot pipeline's `rustos_kernel::boot_riscv64::build_boot_memory_map`).
Handing that map to
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
entry point. The `_start` trampoline (`boot.s`) follows the aarch64
boot protocol's `x0 = DTB` register convention, which real firmware (and
the Pi GPU firmware) populates; note that QEMU's `-kernel <ELF>` path
itself passes `x0 = 0` (it treats the image as bare firmware), so the
verticals that need the board tree embed it at build time rather than
reading the pointer (see [Board-discovered console](#board-discovered-console)).
The trampoline:

1. Masks interrupts (`DAIFSet`).
2. If entered at EL2 (the Pi firmware, or a `virtualization=on` board),
   establishes a **fully-known EL2 state** before dropping to EL1: every
   EL2 control register is *written whole* with the unit-test-pinned
   hand-off values in `rustos_arch_aarch64::el2` — `HCR_EL2 = RW` only
   (EL1 is AArch64, no stage-2, no traps), `CNTHCTL_EL2 = EL1PCTEN |
   EL1PCEN` (EL1/EL0 own the physical counter/timer), `CNTVOFF_EL2 = 0`,
   `CPTR_EL2 =` its RES1 bits (no FP/SIMD trap), `MDCR_EL2 = 0` (no
   debug/PMU traps), and `VPIDR_EL2`/`VMPIDR_EL2` mirrored from
   `MIDR_EL1`/`MPIDR_EL1` — then `eret`s to EL1. The EL2 reset state is
   architecturally UNKNOWN on real silicon (the Pi firmware stub sets
   only `SCTLR_EL2` and SMPEN; QEMU resets everything benignly), and an
   UNKNOWN `HCR_EL2.TVM` traps EL1's first `MAIR`/`TCR`/`TTBR`/`SCTLR`
   write into vector-less EL2 — the silent Pi 4B hang at the MMU switch.
   An `orr` into a live EL2 register is therefore forbidden here. On the
   default `virt` machine the highest EL is already EL1, so this is
   skipped.
3. Writes the known MMU-off `SCTLR_EL1` (`paging::SCTLR_MMU_OFF`, the
   ARMv8.0 RES1 bits only) before the first EL1 data access. The
   register is architecturally UNKNOWN when EL1 is first entered on
   real silicon — QEMU resets it benignly — and an UNKNOWN `EE`
   (big-endian data) or `WXN` bit otherwise wrecks the boot the moment
   it is exercised. The secondary-core trampoline (`smp.s`) does the
   same at its PSCI `CPU_ON` entry.
4. Establishes the boot stack, zeroes `.bss`, and tail-calls
   `rustos_arch_aarch64_main(dtb)`, which forwards to the
   binary-supplied `kernel_main`.

The console (`serial.rs`) routes the boot log by build profile. A
**release build** writes it to the **video display when one is
configured** (see [Framebuffer boot
console](#framebuffer-boot-console-video-first-uart-fallback)) and
otherwise through whatever UART the `console` module currently points
at; before any discovery runs that is the `virt` board's PL011 at
`0x0900_0000`, which QEMU routes to `-serial stdio`. A **debug build**
(`cfg(debug_assertions)`) routes the whole boot-log/debug stream to the
**UART instead** — even when the video console is active — so a serial
capture of a development boot carries the full diagnostic stream while
the screen stays clear for the user-facing session; with no UART
discovered the bounded transmit simply drops the bytes and the screen
is never the debug log's sink.

The single freestanding kernel cannot tell which SD image it was planted
in, so "debug build" is pinned to the **image profile** by building the
kernel in the matching Cargo profile (`tools/xtask` `kernel_build_profile`):
`cargo xtask image --profile debug` compiles a `dev`-profile
(`debug_assertions`-on) kernel whose log diverts to the UART, while the
shippable `cargo xtask image --profile installer` compiles a `--release`
kernel (assertions off) whose log renders on screen. There is no separate
`--release` flag to forget — the image profile decides both the seeded
contents and the kernel's log routing.

### Boot beacons (serial bisection)

The consolidated boot-log line (`KERNEL_BOOT_AARCH64_REACHED`) is emitted
only *after* the MMU is enabled, so a metal boot that wedges before or at
translation-enable — the classic Pi 4B failure — would otherwise leave no
trail at all. To localise such a hang, `boot()` prints ordered,
UART-only **beacons** (`serial::beacon`) at the milestones a pre-MMU /
around-MMU wedge falls between. The pre-MMU window between `2/6` and `3/6`
(building the identity-map masks) is split into finer `2a`..`2c`
sub-beacons, because a metal capture wedged exactly there with `2/6` the
last tag shown.

| Beacon | Printed when | A silent line after the previous beacon points at |
| ------ | ------------ | -------------------------------------------------- |
| `1/6: boot entry` | EL1 reached, FP enabled, MMU off | entry before `boot()`, or `enable_fp_el1` |
| `2/6: mmio discovered` | console/GIC/video/PCIe FDT walk done | the pre-MMU FDT discovery walk (`configure_mmio_from_dtb`) |
| `2a/6: console/gic bases read` | `console::current` + `gic::current` returned | reading the discovered console/GIC bases |
| `2b/6: device gigapage mask configured` | `identity_device_mask` built + `configure_device_gigapages` stored it | device-mask construction / the atomic mask store |
| `2c/6: ram gigapage mask configured` | `identity_ram_mask` built + `configure_ram_gigapages` stored it | RAM-mask construction / the atomic mask store |
| `3/6: identity map built, enabling mmu` | Device + RAM gigapage masks built | identity-map mask construction |
| `4/6: mmu on` (or `mmu enable FAILED`) | translation is live | **the MMU enable itself** — a mis-typed identity map (the metal Pi 4B hang) |
| `4a/6: pcie discovery logged (post-mmu)` | the discovered `brcm,bcm2711-pcie` windows were logged | a metal diagnostic of the PCIe root-complex windows; the windows themselves reach the user-space `pcie_brcm` driver as grants on the discovered node, not a kernel stash |
| `5/6: post-mmu …discovered` | post-MMU `/memory`/timer/PSCI walk done | the full-tree FDT walk that needs the MMU |
| `6/6: entering kernel core` (or `handover REJECTED`) | hand-off assembled | memory-map build / `BootInfo` assembly |

Each beacon writes the UART **only** — never the video console, whose
render lock is UNPREDICTABLE on MMU-off memory and could itself wedge the
boot a beacon exists to trace. The transmit is bounded (`putchar` /
`tx_wait`), so a beacon at a not-yet-discovered or wrong base (beacon 1/6
runs before the FDT walk sets the real Pi base, so on metal it targets the
pre-discovery `virt` default and is silent there) can never spin forever;
beacon 2/6 onward use the discovered base and recover the transmitter on
their first ready poll. An entirely silent serial line therefore points at
"before `boot()` or inside the pre-discovery FDT walk", and the last tag a
capture shows pins the wedge to the step that follows it.

## Board-discovered console

The console MMIO base and register model are **discovered from the
firmware device tree**, not hard-wired (`plans/PI.md` P2). The
host-testable `console` module holds the active `(base, model)` as an
atomic pair (the pre-discovery default is the `virt` PL011 base), and the
freestanding `serial` sink reads it on every transmitted byte.
`console::find_console` / `configure_from_fdt` walk the shared `lib/fdt`
reader for the first node whose `compatible` names a model the port
speaks, preferring the PrimeCell **PL011** (`arm,pl011`) over the BCM2835
AUX **mini-UART** (`brcm,bcm2835-aux-uart`). The node's `reg` is decoded
with its parent bus's cell counts and translated through the ancestor
buses' `ranges` (the shared `fdt::scan_translated` /
`fdt::translated_reg` machinery, `AGENTS.md` §2.2) — on the real Pi 4
tree the UARTs sit under `/soc`, whose one-cell `reg` values are *bus*
addresses (`0x7E20_1000`) remapped to CPU-physical space
(`0xFE20_1000`); an untranslatable node is skipped, never poked at its
raw bus address (`AGENTS.md` §2.9). The two models are one
console abstraction with two register backends — distinct data/status
register offsets and opposite-sense transmit-ready bits — not duplication
(`AGENTS.md` §2.2). The generic `platform::FdtDiscovery` walk emits a
`serial`-class `HwNode` per UART the tree describes, each carrying its
`compatible` bind keys and its `reg` as a capability-gated MMIO resource
— preferring one console is `console` policy, not tree shape.

The runtime walk is safe with the MMU still off: the `lib/fdt` reader
accesses the blob byte-by-byte, so it takes no multi-byte Device-memory
load that would fault without exception vectors (`plans/PI.md` W17).

### Console line bring-up (`uart_init`)

Discovering the UART's *address* is not enough on real silicon: QEMU's
PL011 model powers up enabled, but the metal Pi 4 leaves `UART0` muxed
away from the header and disabled until the kernel programs it — the
board boots with a permanently silent serial port otherwise. Right
after the console is discovered (and before the first log byte) the
boot path runs `uart_init::init_from_fdt`:

1. **Pin mux.** When the tree carries a BCM2711 GPIO controller
   (`brcm,bcm2711-gpio`, bus-translated like every `/soc` peripheral),
   GPIO 14/15 are routed to the PL011 (`GPFSEL1` → `ALT0`) and their
   pulls released (`GPIO_PUP_PDN_CNTRL_REG0` → no pull). The register
   window is sized from the BCM2711 datasheet (`GPIO_REGS_LEN`, `0xF4`),
   not the device tree's historical `0xB4` `reg` length, which predates
   the BCM2711 pull registers. A tree without the controller (QEMU
   `virt`) skips the mux.
2. **Line programming.** The PL011 is disabled, a transmitting frame is
   waited out (bounded by `console::TX_POLL_BUDGET` — a wedged
   transmitter must not hang the boot), and the line registers are
   rewritten in the TRM order: `ICR` clear, `IBRD`/`FBRD` divisors for
   `uart_init::CONSOLE_BAUD` from the pinned 48 MHz reference clock
   (release/installer 9600 → 312 + 32/64; debug 57600 → 52 + 5/64),
   `LCR_H` 8N1 + FIFOs (which latches the divisors), `IMSC` all-masked
   (the console polls), then `CR` re-enable (`UARTEN|TXE|RXE`). The debug
   build runs the faster rate to drain its verbose boot log; the firmware
   `init_uart_baud` is set to the matching rate so the two agree.

The register arithmetic — divisor maths with fail-closed range checks,
the `GPFSEL1`/pull read-modify-write — is pure and host-unit-tested; the
freestanding layer only performs the volatile MMIO (`AGENTS.md` §2.2).
A non-PL011 console (the mini-UART fallback) keeps the firmware's line
state untouched. The same writes run harmlessly under QEMU, so metal
and emulation share one path.

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

### Console input backing (the standard-input stream, fd 0)

The same `console` model backs the **input** half (`plans/PI.md` P6e-2):
`ConsoleModel::rx_ready` decodes the model's receive-status bit (the
PL011's `UARTFR.RXFE` is *set* when the receive FIFO is empty; the
mini-UART's `AUX_MU_LSR_REG` bit 0 is *set* when data is ready), reusing
the same status and data register offsets as the transmit path since they
coincide on both models. `serial::read_console_bytes` drains whatever
input is immediately available into the caller's buffer and stops at the
first byte that is not yet present — it **never busy-waits** for input
(`AGENTS.md` §2.1), so a read with no pending byte is a valid zero-length
short read at the device level. `boot_aarch64` lists this through the
same zero-sized `UartConsole` device (it implements both `ConsoleWrite`
and `ConsoleRead`) in the `BootInfo::with_consoles` console list; the
kernel-core init pipeline then wraps every listed read half in
`BlockingConsoleRead`, which turns an empty device poll into a scheduler
park (`reschedule_current`, the same poll-and-park loop the `wait`
syscall uses) and re-polls when the caller is next dispatched — so a
`stream_read` of fd 0 (whose backing the spawner attaches to a console
entry, `AGENTS.md` §20) **waits** for that console's input rather than
reporting a spurious end-of-input (the backing owns blocking, §20). That
wait is what holds each login session at its `Username: ` prompt
(`plans/PI.md` P11) until the user types.

This is the bootstrap stream **backing** the spawner attaches to fd 0
(`AGENTS.md` §20); it is not a program-facing interface. The receive-bit
decoders are host-unit-tested; the standard-stream layer binds fd 0 to
this backing (`plans/PI.md` P6e-3a), and the `spawn_session_qemu_aarch64`
vertical proves the interactive path end to end: the runner types a
scripted over-long line at the guest's serial input once the blocked
login prints its prompt, and login's fail-closed exit drives `init`'s
reap-and-relaunch cycle.

## Framebuffer boot console (video first, UART fallback)

Console **output** defaults to the attached display; the UART is the
fallback when no video output exists (`plans/PI.md` P7b, `AGENTS.md`
§10). The `video` module brings the screen console up over whichever
display path the firmware tree describes: the `VideoCore` mailbox on
boards whose display pipeline the firmware owns (the Raspberry Pi), or
the `fw_cfg`/`ramfb` fallback on the QEMU `virt` board. On the Pi:

- **Discovery.** `video::find_mailbox` locates the firmware mailbox
  doorbell (`brcm,bcm2835-mbox`) with the same early-returning,
  `ranges`-aware walk as the console and GIC (`fdt::scan_translated`,
  `AGENTS.md` §2.2) — on the Pi 4 tree, bus `0x7E00_B880` →
  CPU-physical `0xFE00_B880`.
- **Bring-up (pre-MMU, by design).** `video::configure_from_fdt` runs
  in the same pre-MMU phase as the console/GIC discovery: with the
  data caches still off, the CPU↔firmware property exchange over the
  shared `rustos-vcmailbox` protocol crate is coherent without cache
  maintenance, and the console state cell is written by the
  single-threaded boot CPU without an atomic read-modify-write (which
  is UNPREDICTABLE MMU-off — the constraint that orders this boot).
  `video::bring_up` first asks the firmware for the display's native
  EDID-derived size (`query_display_size`; `0×0` means no display →
  UART keeps the console) and then allocates a 32-bit surface at
  exactly that size. The doorbell base joins the Device-gigapage mask
  inputs, and the boot audit line records `video_console=true/false`.
- **QEMU `virt` fallback (`ramfb`).** A tree with no mailbox is probed
  for a `qemu,fw-cfg-mmio` node instead. When QEMU was started with
  `-device ramfb`, `video::configure_ramfb` programs the device's
  scan-out — over the shared `rustos-fwcfg` DMA client (`AGENTS.md`
  §2.2), the same protocol definition the display verticals use — to a
  statically-reserved 1024×768 surface in kernel BSS, and records the
  same surface over it (`publish_console`, the shared tail of both
  paths). The fw_cfg base stands in as the "doorbell" Device-mask
  input. No ramfb device (`etc/ramfb` absent), no fw_cfg node, or any
  failed transfer falls back to the UART (fail closed); the headless
  UART-backed verticals are unchanged.
- **Cell-grid attach (post-MMU, by design).** The pre-MMU phase only
  *records* the discovered surface and clears it to a clean background;
  it does **not** build the renderer, because `rustos_fbcon` keeps a
  retained cell grid (below) and that grid is leaked from the kernel
  heap, which is unusable MMU-off (its allocator's atomics are
  UNPREDICTABLE on the MMU-off Device-typed memory the boot CPU runs).
  So immediately after `enable_mmu_and_vectors`, `boot` leaks two
  `[Cell]` grids sized to `video::text_cell_count()` (the discovered
  `columns × rows`) and hands them to `video::attach_console`, which
  builds the `TextConsole`, clears the surface through it, and only then
  publishes `video::is_active` — so console output switches to the
  screen once the grid is live. This mirrors the per-CPU
  `Aarch64ArchStorage` pattern: the caller (which has a heap) owns the
  `'static` storage, the arch crate stays allocator-free. With no
  display discovered `text_cell_count` is `None` and the UART keeps the
  console (fail closed).
- **Rendering — a real `xterm-256color` terminal, in a shared engine.**
  The terminal is not this port's own code: it is the shared,
  architecture-neutral `rustos_fbcon` engine (`lib/fbcon`, `AGENTS.md`
  §2.2 / §2.20 / §2.21), so every arch port renders its display console
  through one definition and this port supplies only the board-specific
  surface discovery above. Shell output is not drawn byte-for-byte: it is
  fed through the **one** streaming ANSI/VT/xterm parser in the tree
  (`rustos_vt::Parser` — no second escape parser), each parsed `Op` mutates
  the retained cell grid, and the dirtied cells are repainted onto the
  scan-out surface once per write. The console therefore
  *interprets*
  escape sequences instead of printing them: SGR rendition with the
  16-colour, 256-colour (`38;5;n`) and 24-bit truecolour (`38;2;r;g;b`)
  models, bold (brightens the base colours) and reverse-video, cursor
  movement and absolute positioning (`CUP`), the erase operations
  (`ED`/`EL`), the scroll region (`DECSTBM`) and explicit scrolling
  (`SU`/`SD`), the alternate screen, and the saved cursor. Glyphs are
  the shared Inconsolata EX coverage atlas (`rustos_font::atlas` — one font
  definition) at an integer scale chosen from the display height
  (`height / 1080`, clamped to 1…4: 1080p → 1×, 2160p → 2×), packed
  `0xFF00_0000 | (r<<16) | (g<<8) | b` — correct on both the mailbox
  (`Bgra8888`) and ramfb (`XRGB8888`) surfaces, whose bytes coincide.
  The engine keeps a **retained character-cell grid** (`rustos_vt::Cell`
  per position): each write mutates the grid and the dirtied cell rect is
  repainted from it **once** at the end of the write, so reaching the
  bottom margin performs a real terminal scroll (grid `copy_within`), not a
  ring-wrap — and a burst that scrolls many lines touches the framebuffer
  once, never once per line (the per-line pixel copy made a large listing
  monopolise the CPU for seconds on the Pi 4, starving the buffered serial
  drain). The grid is what lets the console restore the primary screen
  when a full-screen program leaves the alternate screen (below).
  `rustos_fbcon`
  (and, through it, `lib/vt` and `lib/font`) is depended on
  `default-features = false`: `lib/vt`'s `Vec`-returning `encode*` helpers
  ride its default-on `alloc` feature, while `Op` itself owns no heap (the
  OSC title is a bounded inline `Title`), so the allocator-free parser is
  all the minimal QEMU test bins link (the same discipline as the
  `rustos-font` atlas-only dependency). The parser is
  total, so a malformed or unrecognised sequence is consumed without
  disturbing the screen; a Unicode scalar the atlas cannot draw renders
  `?`. Attributes with no bitmap rendering (underline/italic/blink/dim/
  strike) are documented degrades; the **alternate screen** (`CSI ? 1049
  h`/`l`) is fully honoured — entering saves the primary-screen cursor and
  shows a cleared alternate grid, and leaving restores the primary screen
  from its grid exactly, so quitting `top` or an editor returns the shell
  screen it covered. The cursor is a software-drawn reverse-video block over
  its cell (there is no hardware cursor on the scan-out surface), honouring
  DECTCEM show/hide. After the MMU and caches come on, each write cleans the touched
  scanlines to the point of coherency (`dc cvac` + `dsb`) so the
  firmware scan-out sees them; rendering is serialised by a private
  DAIF-masking spinlock (deliberately not `lib/sync` — feature
  unification across the single aarch64-none test-matrix build would
  compile its alloc-backed `epoch` module into the minimal,
  allocator-free QEMU binaries; the carve-out is documented at the lock).
- **Routing.** The **boot-log** path (`serial::ConsoleWriter`, the log
  sink) routes by build profile. A **release build** renders to the
  screen when `video::is_active` and falls back to the UART otherwise.
  A **debug build** (`cfg(debug_assertions)`) routes the whole
  log/debug stream to the **UART instead** — even when the video console
  is active — so a serial capture of a development boot carries the
  full diagnostic stream while the screen stays clear for the
  user-facing session; with no UART discovered the bounded transmit
  drops the bytes and the screen is never the debug log's sink. The
  **stream** path installs one console (`plans/PI.md`
  P11): `boot_aarch64` installs `[VideoConsole]` through
  `BootInfo::with_consoles` when the framebuffer console is active — the
  UART is then the debug log line only, with no console and no session
  that would draw over the log stream — else `[UartConsole]`.
  `VideoConsole` writes through
  `video::write_bytes`; its input half is the shared `VIDEO_KEYBOARD`
  queue (`rustos_kernel_core::ConsoleInputQueue`), which is both the
  console's `ConsoleRead` half (drained by a video-login `stream_read`)
  and its `ConsoleInput` half (fed by the `console_input` syscall a
  keyboard-input driver issues after decoding a directly attached
  USB-HID / PS/2 keyboard). Until a keyboard driver pushes anything the
  queue is empty, so the reader parks at its prompt rather than
  borrowing the serial line — the video login takes input only from its
  own keyboard, never the UART. (The keyboard-input driver and its
  USB-HID/xHCI hardware path land on the Pi-metal P10 track; this is the
  kernel-side delivery seam it pushes into.) `UartConsole`
  (`serial::write_console_bytes` / `read_console_bytes`) is the UART's
  own stream backing — the login console of a serial-only boot — and
  never touches the display. The UART transmit wait is **bounded**
  (`console::tx_wait`): a transmitter that never drains — e.g. a
  flow-blocked PL011 still attached to the Bluetooth chip — is declared
  wedged after `TX_POLL_BUDGET` polls and bytes are dropped (one cheap
  poll each, recovering the moment the FIFO drains) rather than hanging
  the kernel on its first log line (`AGENTS.md` §2.1).

Fail closed (`AGENTS.md` §2.9): no mailbox node and no ramfb device, a
detached display, or any rejected/malformed firmware answer leaves the
UART as the console. The discovery, bring-up (over the
protocol-faithful mock firmware — QEMU does not model the `VideoCore`),
geometry policy (including the fixed ramfb mode), and renderer are
host-unit-tested; rendering on a real HDMI display is an on-metal
acceptance item like the rest of the Pi peripherals.

### Interactive QEMU session (`cargo xtask run`)

`cargo xtask run --target aarch64-rpi [--profile debug|installer]
[--cpus N] [--firmware <dir>]` builds the requested platform image
(exactly as `cargo xtask image`) and boots it interactively on
`qemu-system-aarch64 -M virt`. The Pi-linked kernel inside the image
loads at `0x8_0000` (not RAM on `virt`), and QEMU's ELF `-kernel` path
passes no DTB, so `run` additionally builds the **`virt`-board form of
the same production kernel crate** (`RUSTOS_KERNEL_BOARD=virt` →
`aarch64-virt.ld`) and boots it as an arm64-`Image`-wrapped raw binary
(`rustos_mkimage::elfflat::build_virt_boot_image`), which QEMU loads at
the `virt` link address and enters with the generated device tree in
`x0` — the same hand-off shape as the Pi firmware. The session attaches
the image as the virtio-blk root disk, `-device ramfb` for the windowed
display the boot console renders on, and virtio keyboard + mouse
devices for input from the QEMU window (the autoloaded
`drivers/input/virtio_kbd` bundle the image ships binds the discovered
virtio-input nodes and injects decoded keys into the input-focus
arbiter). The invoking terminal is the guest's serial console: type the
encrypted-root unlock passphrase there (`root` for the `debug` profile;
empty — just press Enter — for `installer`). The session has no
deadline; it ends when the QEMU window is closed or the guest powers
off.

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
(region 0 = distributor, region 1 = CPU interface), decoding each region
with the parent bus's cell counts and translating it through the
ancestor buses' `ranges` (`fdt::translated_reg`) — on the real Pi 4
tree the GIC-400 sits under `/soc` with one-cell *bus* `reg` values
(`0x4004_1000`) remapped to the CPU-physical bases; an unrecognised,
absent, or untranslatable controller leaves the fail-safe default in
place (`AGENTS.md` §2.9). The generic `platform::FdtDiscovery` walk emits an
`InterruptController` `HwNode` carrying the discovered `compatible` bind
keys and both register windows as capability-gated MMIO resources.

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
live-timer interval from it, logging `timer_hz_from_tree` and the
resolved rate itself as `timer_hz_hex`. So the QEMU `virt` board's
host-derived rate and the Raspberry Pi 4's 54 MHz crystal both flow
through one path with no `cfg(board)` fork (`AGENTS.md` §17.2 / §2.2).

> **Historical (pre-Increment-C) note.** The bring-up chronicle below was
> written against the in-kernel `open_controller`/`VideoCoreFirmwareReset`
> diagnostic path. Increment C (the full swap) moved that xHCI + firmware
> bring-up into the floor `drivers/bus/*` crates, removing events
> `4102`/`4104`/`4106`/`4107`/`4109`/`4110`/`4114`/`4118`/`4122`/`4123`/`4124`/
> `4125`/`4126` and the `open_controller`/`wait_for_caps_ready`/
> `VideoCoreFirmwareReset` symbols from the live path. The PCIe-side findings
> (timer rate, the outbound-window root cause, the post-link-up bridge enable)
> remain accurate for `drivers/bus/pcie_brcm`.

The same resolved rate drives every `kernel_arch::busy_delay_us` settle
the in-kernel bring-up uses (the BCM2711 PCIe reset/SerDes/link-training
waits): `busy_delay_us` spins `CNTPCT_EL0` against this rate. The metal
capture read `timer_hz_from_tree=false timer_hz_hex=0x337_f980` — exactly
the Pi 4's 54 MHz crystal — so `CNTFRQ_EL0` is **correctly** programmed
and a mis-programmed-rate `busy_delay_us` over-wait is ruled out as the
cause of the multi-second USB bring-up pause.

A `4116` "bring-up delay timing measurement" measures the whole bring-up
chain: the keyboard service brackets it with `kernel_arch::read_cntpct`
and tallies, in its `GenericTimerDelay`, how long the code *asked* to
wait (`requested_us_hex`, over `delay_calls_hex` calls), reported against
the `CNTPCT_EL0`-measured span (`counter_elapsed_us_hex`, same
`timer_hz_hex` rate). The capture read `requested_us_hex=0x57030`
(≈356 ms over `delay_calls_hex=0x103`=259 calls) yet
`counter_elapsed_us_hex≈14.3 s` at the correct 54 MHz — so ≈14 s of
*real* time elapsed with only ≈356 ms of it in `busy_delay_us`. The
counter is sound; the seconds are code-side, outside the delays.

`4116` alone cannot say *where* in the chain those seconds go, so per-line
log timestamps were added: `SerialSink::write_event` prefixes every line
with `[<secs>.<millis>]`, a monotonic `CNTPCT_EL0`-derived stamp
(`kernel_arch::uptime_ms`, scaled by `CNTFRQ_EL0` — the same counter/rate
`busy_delay_us` spins against; epoch unspecified, only differences
matter), and `build.rs` emits a `KERNEL_BUILD_ID` (git short hash +
`+dirty` + a `SOURCE_DATE_EPOCH`-aware build epoch for §19.3) logged as
the `build_id` field on the `4097` boot line so a capture proves which
build is running. The timestamped capture (with `build_id` confirming the
current image) was **decisive** and corrected the earlier, un-timestamped
attribution:

- The caps-readiness wait (`4108`→`4109`) takes only ~0.35 s: the
  `wait_for_caps_ready` **elapsed-wall-time** bound (`CAPS_READY_BUDGET_US`
  ≈256 ms via `Delay::now_us`, `CNTPCT_EL0`-backed on metal) works as
  intended. `4109 polls_hex=0x100` is now 256 *fast* reads inside that
  ~256 ms budget (~1.3 ms each) — the BCM2711 master-abort returns the
  `dead_dead` poison **quickly**, not the ~54 ms first *inferred* by
  dividing the un-split 14 s figure by 256. That inference was wrong; the
  wall-time bound is correct and is retained (a poll-count budget would
  still be a §2.16 defect if reads were ever slow).
- The ~14 s pause is almost entirely **inside `BrcmPcieRc::bring_up`**:
  the gap between the RC-register-window map (`4105`,
  `phys_base=fd50_0000`) and `pcie root-complex link trained` (`4101`) is
  ~11.2 s, with no log line between them. Everything after bring-up
  (config scan, BAR map, firmware-version wait, caps wait) sums to ~3 s of
  ordinary per-line logging cost.

The `4117` per-phase split (`BringUpTiming` from the `Delay` clock,
logged at `AGENTS.md` §15.7 — measure, don't guess) localised the ~11 s
first to the **reset phase**, then the reset sub-spans pinned it to the
**first access to the MISC register block** (`0x4xxx`). The first attempt
read link status before powering the SerDes and stalled there; powering
the SerDes (a `MISC_HARD_PCIE_HARD_DEBUG` `0x4204` write) **first** only
moved the stall — the next metal capture then showed *both* the SerDes
write (the SerDes-write span ≈10.8 s) and the following status read
(the status-read span ≈21.6 s, two timeouts) master-aborting, doubling the
pause to ~33 s. That refuted the SerDes-IDDQ theory: it is not the SerDes
state — *every* early MISC access master-aborts, and adding one added a
timeout.

The real gate is the controller reset. The BCM2711 holds the PCIe
controller core off at OS entry, and a MISC-block access does not
complete until the always-accessible RGR1 bridge `sw_init` reset
(`0x9210`) has been cycled — which is exactly why the **same**
`MISC_PCIE_STATUS` read costs ~8 µs in the configuration phase (the reset
having run by then) yet ~10.8 s before it. The BCM2711 PCIe bring-up
sequence never touches a MISC register before cycling the bridge
`sw_init` reset, and only *then* clears the SerDes IDDQ.

**Fix:** `BrcmPcieRc::reset_controller` releases the bridge `sw_init`
reset on the always-accessible `0x9210`, bringing the core and its MISC
block online, before any MISC access. The ~10.8 s pause is host-proven
fixed and confirmed gone on metal (`reset_swinit_us`/`reset_settle_us` ≈
microseconds). The gentlest **no-touch-probe** bring-up (below) does
**not** re-assert a fundamental reset or toggle the SerDes IDDQ.

### The VL805 firmware: leave the boot firmware's state alone

The keyboard path treats VL805 firmware as boot-firmware-owned. The user's
known-good capture on the same board/SD/firmware shows the board loads
the VL805 firmware before the OS, USB works in the pre-boot firmware menu,
and no runtime VL805 reload is needed. The RustOS path therefore keeps the boot firmware's
PCIe/VL805 state intact wherever it can — `reset_controller` only releases the
reset the previous stage left asserted (below), never re-asserting a
fundamental reset that could drop resident firmware. A `NOTIFY_XHCI_RESET`
reload is still issued as a **best-effort** fallback when config `0x50` reads
`0`, but the latest metal capture showed that request timing out
(`4108`/`4121`) and leaving `0x50` at `0` with the BAR still `dead_dead` — so
its outcome no longer gates the bring-up (see the firmware-version gate,
`4123`, below).

The bring-up uses the gentlest no-touch sequence:

- `reset_controller` **releases** the bridge `sw_init` the previous boot
  stage left asserted (it does **not** re-assert a fundamental reset or
  toggle the SerDes); `train_link` deasserts the already-asserted `PERST#`
  once. The `reset_releases_sw_init_without_re_asserting_a_fundamental_reset`
  test guards the release-only, no-re-assert sequence.
- `open_controller` waits for the VL805's **XHCI MCU firmware version**
  (config `0x50`) to read non-zero (`4118`), using configuration space — the
  same register the vendor firmware-init sequence checks — rather than the
  master-aborting BAR. If the version stays `0`, the best-effort
  `NOTIFY_XHCI_RESET` reload is issued once; either way the bring-up records
  the firmware-version gate decision (`4123`) and **proceeds** to the BAR wait
  (`4109`) and `Xhci::open` — the authoritative xHCI liveness signal and the
  real fail-closed gate. Config `0x50` is a VL805 vendor convenience, not the
  controller's readiness signal, so a `0` there (or a dropped reload) no
  longer aborts the bring-up before the controller's own capability block is
  probed.

The `UART` does **not** touch the VL805: the Pi 4 debug serial is the
BCM2711's own PL011 / mini-UART on GPIO14/15, an on-SoC peripheral with
no path to the PCIe root complex, so logging cannot perturb the
controller or account for the pause.

This is host-proven (the driver/usb_keyboard tests assert the release-only
`PERST#` sequence and the no-touch firmware path), but the live keyboard
enumerating on metal is an on-metal acceptance item (no `raspi4b` in QEMU —
QEMU models no Pi PCIe/USB). A healthy capture shows a live
`CAPLENGTH`/`HCIVERSION` at the BAR (`4109 ready_hex=1`, `4107`) and the
keyboard enumerating, whether or not config `0x50` ever read non-zero; a BAR
that stays `dead_dead` (`4109 ready_hex=0`) means the controller never decoded
and the bring-up fails closed at `Xhci::open`.

**Inbound-window lead (raspberrypi/firmware #1617, then #1495).** A
maintainer report (#1617) suggested `VideoCore` loads the VL805 firmware
over PCIe **through an inbound DMA window**. A now-removed post-program
read-back of that window (a former `4119` event) read it back
byte-identical to the known-good *runtime*
`IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`. But matching that *runtime*
window does not prove the window matches what `VideoCore` assumes at the
**firmware-load** moment: pftf/RPi4 issue #1495 establishes that the
`NOTIFY_XHCI_RESET` load *assumes* a particular `RC_BAR2` state instead of
reading it back, so a window bring-up reprograms away from what `start4.elf`
set at power-on makes the reload honoured-but-no-op (or, with newer
co-processor firmware, makes the mailbox exchange itself time out — the
`4108 … reason=timeout` symptom). The standing change for this lead: a
one-shot `4120` capture of the inbound window **as the previous boot stage
left it** (`BrcmPcieRc::entry_inbound_window`, sampled before bring-up
touches `RC_BAR2`), which both lets a metal run see the firmware's own
window and drives bring-up to **preserve a firmware-configured `RC_BAR2`**
(a non-zero size field) rather than overwriting it, honouring `VideoCore`'s
assumed state for the load. (The matching post-program read-backs were
removed once bring-up was metal-confirmed: on real BCM2711 silicon reading
those MISC registers after the link trains stalls for seconds while the
in-kernel bring-up holds the CPU, and with the link up they added no
functional value — `AGENTS.md` §2.14 / §2.16.) Decisive metal datapoint:
the `4120` capture identical to the known-good runtime window with
`4118 fw_version=0` pins the residual on the firmware handoff below.

The residual is therefore **outside the PCI path**, in the firmware
handoff. RustOS's own aarch64 boot/arch code never writes
`RGR1_SW_INIT_1` (only `pcie_brcm::reset_controller` does, *after*
sampling `entry_rgr1_sw_init`), so the metal `entry_rgr1_sw_init=0x3`
(PERST + bridge `sw_init` asserted) is `start4.elf`'s handoff state. A
working boot chain inherits or obtains resident firmware before its xHCI path
and reads config `0x50` first, **skipping** any runtime reload (on the
user's working setup the VL805 firmware is already resident,
`0x50 = 0x000138c0`). The chosen in-tree experiment (`AGENTS.md` §15.7; the user declined the
chain-load route) is the **gentlest possible no-touch-probe**
bring-up: `reset_controller` only *releases* the bridge `sw_init` the
previous boot stage left asserted and does **not** re-assert a fundamental
reset or toggle the SerDes `IDDQ`, so any resident VL805 firmware is left
untouched; `train_link` deasserts the already-asserted `PERST#` (the single
firmware-(re)load edge), and the `NOTIFY_XHCI_RESET` reload is a fallback
issued only when the `4118` firmware-version gate (config `0x50`) stays `0`.
That fallback now first issues a **runtime mailbox liveness probe** (a
benign `GET_FIRMWARE_REVISION` read over the same post-MMU transport,
logged as `4122`) before the reload, because the bare `4108 reason=timeout`
cannot tell a broken post-MMU mailbox path from `VideoCore` silently
dropping only the xHCI tag: `4122 probe_outcome=ok` with a non-zero
`firmware_revision_hex` proves the transport works and pins a following
`4121 timeout_stage=response` on the firmware dropping the tag, whereas
`4122 probe_outcome=timeout` pins it on the mailbox environment itself
(`AGENTS.md` §15.7). The mailbox property framing itself has been audited
against the VideoCore protocol (raspberrypi.stackexchange.com #133040): every
tag's value-buffer length word is sized to `max(request, response)` words, so
the classic "`0`-byte buffer, firmware cannot reply" defect cannot occur, and
`find_tag` reads the per-tag response bounded by that buffer and fails closed
on a reply that claims more — confirming the `4122 ok` / `4121 response`
timeout is the firmware dropping the tag, not a response-length/sizing fault.
A read-only post-reload `4124` capture then showed `bridge_secondary_status=0`
(no Received Master/Target Abort), so `VideoCore`'s firmware-load is **not**
master-aborting on the root-port bridge — the downstream VL805 path is
reachable, not a bus-config dead end. That left one untested variable in the
`4121 timeout_stage=response`: **how long we actually waited.** The reload
busy-polled `DEFAULT_POLL_BUDGET` (1,000,000) iterations, which on metal was
only the ≈400 ms `4122`→`4121` gap — well under the **full second** the
vendor bring-up allows the same property call. `NOTIFY_XHCI_RESET`
is a long operation (`VideoCore` fetches the VL805 blob and pushes it over
PCIe), so a `response`-stage timeout at ≈400 ms is as consistent with "not
finished yet" as with "dropped". The reload mailbox therefore now uses a
dedicated `FIRMWARE_RELOAD_POLL_BUDGET` (ten times the quick-read budget,
≈4 s of metal wall time, comfortably above that 1 s) and `4121` reports the
`CNTPCT_EL0`-measured `wait_elapsed_us_hex` so the iteration→time mapping is
observed, not assumed (`AGENTS.md` §15.7). The follow-up metal capture
**confirmed** this fix: `4121 timeout_stage=none`, `last_status=1`,
`wait_elapsed_us≈0x16266` (≈90 ms), and `4108` now logs the reload as
*reloaded* (success) — the prior give-up was indeed premature.

**Inbound SCB window unsized — `MISC_CTRL.SCB0_SIZE` (current lever).** With
the reload now completing yet `fw_version` still `0` and the controller still
stuck in `CNR` (`4101 stage=reset_self_clear usbsts=0x815`), the residual is
`VideoCore`'s firmware-load running without landing the blob. Comparing our
`pcie_brcm` `bring_up` against the **known-working** BCM2711 PCIe bring-up
sequence found the one concrete divergence: both
program `MISC_CTRL.SCB0_SIZE = ilog2(round_pow2(region)) - 15` (bits
`[31:27]`, mask `0xf800_0000`) to size the inbound SCB (PCIe→system-memory)
decode window to the DMA region, **unconditionally** on the BCM2711 — while our
`bring_up` set every other `MISC_CTRL` bit but left `SCB0_SIZE` at its reset
default. An undersized inbound decoder silently drops a PCIe→memory DMA past
that small window while config reads, enumeration, BAR assignment and the
outbound (CPU→PCIe) path all still succeed — exactly the observed signature
(the `4124` snapshot showed **no** master-abort, so the drop is on the
inbound/SCB side, not the root port). `VideoCore`'s `NOTIFY_XHCI_RESET`
firmware-load reaches system memory through this inbound window, so the
unprogrammed `SCB0_SIZE` is a grounded reason the reload completes yet the
firmware never becomes resident. `bring_up` now programs `SCB0_SIZE`
(`encode_scb_size`, `0x11` for the Pi's 4 GiB viewport, matching the `RC_BAR2`
size encoding) and the `4120` inbound capture carries a `misc_ctrl_hex`
field so a metal capture confirms it. Decisive metal datapoint: `fw_version`
(`4118`/`4114` `0x50`) going non-zero — or the keyboard enumerating — proves
the undersized inbound window was the blocker and the in-tree path is complete;
still `0` with `misc_ctrl_hex` showing `SCB0_SIZE` (bits `[31:27]`) = `0x11`
pins the residual on the boot-chain firmware handoff below.
Because the reload outcome no longer aborts the bring-up, the metal log now
always reaches the BAR capability-block readback (`4123` then
`4109`/`4107`/`4114`/`4106`) — the authoritative datapoint the old
abort-at-`4108` path never captured. Decisive metal outcome: a live
`CAPLENGTH`/`HCIVERSION` at the BAR (`4109 ready_hex=1`) with the keyboard
enumerating proves the controller's firmware is resident regardless of config
`0x50`, and the in-tree path is then complete; `4110`/`4114`
`vl805_fw_version_hex` going non-zero (with a live `CAPLENGTH`) likewise proves
`start4.elf` left the firmware resident and our earlier reset was destroying it;
still `0`/`dead_dead` proves the bare-metal handoff never loads it, leaving
a boot-chain / firmware matter (a chain-loader that
loads the VL805 firmware once after PCIe config, or keeping PCIe up across
the handoff) as the only remaining path. Metal-only either way (QEMU models
no Pi PCIe/USB).

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
  identity-maps the low GiBs with 1 GiB L1 block descriptors under two
  configured masks: the gigapages named by the Device mask
  (`paging::configure_device_gigapages`) are Device for the board's
  UART/GIC MMIO, the gigapages named by the RAM mask
  (`paging::configure_ram_gigapages`) are privileged-executable Normal
  for the kernel image and stack, and a gigapage in **neither** mask is
  left *invalid* — on real silicon a Normal write-back executable
  mapping of unbacked address space invites the core's speculative
  fetches onto bus windows nothing answers, which wedged the metal
  Pi 4B the instant translation enabled while QEMU (which answers every
  address) stayed green. The Device mask defaults to GiB 0 (the `virt`
  MMIO window) and is derived at boot from the *discovered* console/GIC
  bases minus the kernel image's own gigapages
  (`paging::identity_device_mask`) — on the Pi 4 that types gigapage 3
  (the BCM2711 high-peripheral window) Device and keeps gigapage 0,
  which holds the kernel at `0x8_0000`, Normal and executable. The RAM
  mask defaults to *all* slots (host tests and the QEMU integration
  kernels keep the historic everything-Normal map) and is derived at
  boot in two phases (`paging::identity_ram_mask`): pre-MMU from the
  facts in hand — the kernel image's extent, the firmware DTB blob, and
  the firmware scan-out surface — then widened with the `/memory`
  window once the post-MMU walk discovers it, both re-installing the
  mask for later-built process spaces and installing the new gigapages
  into the live boot space (`AddressSpace::ensure_identity_gigapage`,
  an invalid→valid L1 update that needs only a store barrier, no TLB
  invalidation). Every later identity window is *derived from those
  masks*, never a board constant: PID 1's spawn space (`init_spawn`)
  and each runtime-spawned child's (`spawn_producer`) size their
  identity map, their physmap bound, and their stack-arena grow bound
  with `paging::configured_identity_gigapages` (highest Device or RAM
  gigapage + 1 — 2 GiB on `virt`, 4 GiB on the Pi 4). The former
  hard-coded 2 GiB `virt` window left the Pi 4's gigapage-3 UART/GIC
  out of PID 1's root, silencing the metal console the instant
  `spawn_init` switched to it; an empty window or one reaching the
  64 GiB user bias fails the spawn closed. `map_4k` adds finer
  mappings. Before `switch` runs, the
  boot path sweeps the just-written tables to the point of coherency
  (`PageTablePool::clean_invalidate_to_poc`, `dc civac` per
  `CTR_EL0`-decoded line): the tables were written with the data cache
  off but the walker reads them back *cacheable* the instant translation
  enables, and a stale firmware-era line over the pool would shadow the
  real descriptors on real silicon (cache-less QEMU cannot show it —
  the same residue hazard early boot code invalidates its idmap tables
  for). The pool's own allocation counter is translation-aware
  (`PageTablePool::alloc`): with the MMU off it advances by a plain
  load + store, never an atomic read-modify-write, because LDXR/STXR
  exclusives are only architecturally guaranteed on cacheable Normal
  memory — on the BCM2711's MMU-off Device-nGnRnE accesses the
  exclusive monitor never grants them and a `fetch_add` retry loop
  spins forever (the metal Pi 4B hung exactly there while QEMU's
  always-granting monitor stayed green). MMU-off allocation is
  single-threaded by construction — only the pre-SMP boot CPU runs
  Rust with translation off and a pool in hand — and once translation
  is live the counter reverts to `fetch_add`. `switch`
  programs `MAIR_EL1`/`TCR_EL1`/`TTBR0_EL1`, orders the pre-MMU table
  stores with a full-system `dsb sy` (MMU-off stores are Device-nGnRnE,
  outside the inner-shareable domain an `ish` barrier covers), and
  installs the **whole**
  known `SCTLR_EL1` value (`paging::SCTLR_MMU_ON`: RES1 + translation +
  data/instruction caches, after an `ic iallu`), never OR-ing `M` into
  the live register — that would carry the UNKNOWN EL1 reset bits
  (`WXN`, `EE`, …) into translated execution, the silent pre-vectors
  hang real Pi 4 hardware exhibited while QEMU stayed green. The data
  cache is enabled deliberately: the allocator/scheduler LDXR/STXR
  exclusives are only guaranteed on cacheable Normal memory, and the
  framebuffer console already cleans its writes to the point of
  coherency.
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
  GICv2 distributor / CPU-interface / SGI driver. The common trampoline
  saves the interrupted GP registers **and** the per-exception return
  state (`ELR_EL1`/`SPSR_EL1`/`SP_EL0`) into a 288-byte frame, writing
  them back before `eret`. Saving the return state — not relying on the
  live system registers — is what lets a task that is suspended
  mid-handler resume correctly after a cooperative context switch ran
  another task in between: the other task's `eret`/trap overwrites those
  registers, so the resuming exception must restore its own copy or it
  would return to the wrong task's PC/stack (the SP2 resumable
  user-kthread runtime; a parked `wait`/`yield` depends on it). The first
  31 frame slots are the `[u64; SAVED_GPRS]` view `syscall_entry` reads,
  so their offsets are fixed.
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
  `rustos_kernel::boot_aarch64::boot`, which discovers the console + GIC
  bases MMU-off from the embedded `virt` device tree and derives the
  identity map's Device and RAM gigapage masks from them, enables the stage-1
  identity MMU (512×1 GiB gigapages over a static boot `PageTablePool`,
  then `switch`) + EL1 vectors, discovers the rest of the board
  (`/memory`, timer, PSCI), builds the `BootMemoryMap`, installs the discovered-UART
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
- `rustos-test-stack-guard-qemu-aarch64` — **the guard-page fault-form
  works** (`plans/PI.md` G1): `AddressSpace::split_block` shatters the
  coarse identity block covering a dedicated guard page down to 4 KiB
  pages, then that single page is `unmap`ped + `flush_page`d through the
  Arch HAL; reading it raises a data abort the `fault` handler confirms.
  A sentinel write/read-back before the unmap proves the split preserved
  the live mapping.
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
the flattened device tree the firmware hands the kernel. The
device-tree *parser* is the shared `lib/fdt` crate (one parser for every
arch, §2.2); `kernel/arch/aarch64::fdt` layers the aarch64-specific
boot-path queries on it (the `/psci` `method` — `hvc`/`smc`, the conduit
the Stage W6 secondary-core bring-up calls — and the `/timer` node's
optional `clock-frequency` counter-rate override, PI Stage P4).

Hardware-tree emission is **generic** (PLAN.md Stage 4.HW): the walk
emits a node for every device the tree describes, with no per-device
list to grow. Every node carrying a `compatible` property becomes a
hardware-tree node whose match keys are that property's strings in
devicetree (most-specific-first) order — the keys `devmgr` resolves
driver bind tables against (`AGENTS.md` §18.3); `/memory` nodes
(classified by `device_type`) become `Memory` nodes. Each `reg` entry is
decoded with the parent's `#address-cells`/`#size-cells`, translated
through every ancestor bus's `ranges` into a CPU-physical address, and
emitted as a capability-gated MMIO resource — an entry an ancestor
cannot translate is dropped, never emitted untranslated. Each
`interrupts` specifier (the three-cell GIC form both supported boards
use) becomes a capability-gated (`CAP_IRQ_BIND`) IRQ resource. The
device class is derived from the node's own data (`device_type`, the
`interrupt-controller` marker, or the spec-recommended generic
node-name stem), defaulting to `Other`; class is advisory — binding is
by match key. Interior buses (e.g. a `simple-bus` `/soc`) are emitted
as `Bus` nodes before their children, so the flat stream reconstructs
the tree shape, and a node describing nothing bindable (no
representable match key, not memory) is not emitted. Two nodes carry a
per-device augmentation only the platform's tree can size:

- the VideoCore firmware mailbox (`brcm,bcm2835-mbox`, PI Stage P7) — a
  `Dma` request for a one-page property-buffer carve bounded by the
  30-bit VideoCore aperture, which `drivers/display/rpi_hvs::wiring`
  binds; and
- the BCM2711 PCIe host bridge (`brcm,bcm2711-pcie`, PI Stage P10) —
  the host bridge the VL805 xHCI (the USB-A ports) sits behind. It
  carries two windows the VL805 wiring needs, read from the device
  tree, never a board constant (§18.5):
  - the **inbound-DMA aperture** it grants devices behind it, from the
    node's `dma-ranges` (`fdt::dma_ranges_aperture`: the 2-cell parent
    CPU base and size decoded, and the low 64 bits of the 3-cell child
    PCI address kept as the inbound viewport's far-side base). It is
    emitted as `HwResource::dma_translated(top, len, pcie_base)` where
    `top` is the *exclusive* upper bound of the reachable CPU-physical
    window, `len` its extent, and `pcie_base` the PCIe-space address the
    inbound viewport is programmed at, carried in the resource's
    translation field. On a real Pi 4 this viewport is **translating**
    (`IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`: a non-zero `pcie_base`),
    not anchored at PCIe address 0 — so the inbound DMA path is the
    translated case, not a special one. The `pcie_brcm` wiring forwards
    this aperture **verbatim** onto the published VL805 node so the
    kernel's grant-coverage check admits it (the `Dma`→`Dma` rule requires
    the identical translation) and the matched driver's `dma_alloc`
    resolves a device-visible bus address through the same viewport
    (`kernel/core::devres::translate_device_addr`); bring-up programs the
    inbound BAR from `pcie_base`/`len`.
  - the **outbound MMIO window** it forwards to PCIe memory space, from
    the node's `ranges` (`fdt::outbound_mmio_window`: the first
    memory-space entry's `phys.hi` space code is checked, the 64-bit
    PCIe base read from `phys.mid`/`phys.lo`, the CPU base and size
    from the parent cells). It is emitted as
    `HwResource::bus_window(cpu_base, size, pcie_base)` (CPU
    `0x6_0000_0000` → PCIe `0xC000_0000`, 1 GiB on the Pi 4) — a
    capability-grant request distinct from the controller's own `reg`
    MMIO so the wiring can both program the root complex's outbound
    window and translate an enumerated BAR back to a CPU-physical
    address.

  The bridge's own controller/config (ECAM-access) window flows through
  the generic translated-`reg` path, not the augmentation.

The walker is host-tested against the shared DTB fixtures (the `virt`-
and Pi-shaped trees, a nested `ranges`-translating bus, the PCIe bridge
with its `dma-ranges` and outbound `ranges`, fail-closed cases) and
exercised by the port's `passes_arch_hal_conformance_suite`.

## VideoCore mailbox service (user space, P10 D3)

The `VideoCore` firmware property mailbox is a **user-space service driver**
(`drivers/bus/mailbox/vcmailbox`, `AGENTS.md` §4): the §18.6 bootstrap floor
stays storage-only and the mailbox is reached, like every other device,
through discovery and a capability-gated service. The discovered mailbox node
above (the doorbell `reg` window plus its one-page `Dma` carve request) is what
the service binds.

- **The service.** Autoloaded by `devmgr` when the mailbox node is discovered,
  it builds an `RtDriverHost` from its kernel-issued grants, maps the doorbell
  window (`sole_register_window` + `mmio_map`), carves the property buffer
  (`dma_alloc`), and builds the BCM2711 transport (`lib/vcmailbox::MmioMailbox`)
  over them. The kernel carves coherent DMA, so the program supplies no
  architecture-specific cache shim and names no board address (`AGENTS.md`
  §2.20). It then `call_create`s a restricted-sender call endpoint and serves
  forever: `call_recv` → exchange → `call_reply`.
- **The protocol.** `lib/abi::mailbox_ipc` is the wire contract for the
  well-known `MAILBOX_ENDPOINT`: a request is the 32-word property buffer
  little-endian, a reply is a status word followed by the firmware's response
  buffer (a fail-closed status-framed error otherwise, `AGENTS.md` §5.4 / §2.9).
  The board-neutral server transform (`mailbox_ipc::serve_request`) lives once
  in `lib/abi` and is shared by the service (`AGENTS.md` §2.2).
- **The client.** A driver that needs a firmware exchange — the VL805 USB
  firmware reload (`drivers/bus/usb/vl805`) — obtains a `MailboxChannel` from
  its host (`DriverHost::mailbox`); the rt-backed `RtDriverHost` implements it
  by marshalling `exchange` over `ipc_call(MAILBOX_ENDPOINT, …)`, so the VL805
  driver runs unchanged in user space. The endpoint's send gate (`CAP_MAILBOX`)
  is enforced kernel-side; a caller without it fails closed (`AGENTS.md` §5.2 /
  §5.4).
- **Bind identity.** The mailbox `compatible` string and the service's
  `BIND_KEYS` are one definition in `lib/vcmailbox` (`MAILBOX_COMPATIBLE`), so
  the discovery key here and the autoload match key can never diverge
  (`AGENTS.md` §2.2).
- **Install (P10 D4).** The signed bundle ships in the flashable image's
  read-only `/System/Drivers/` store at `Drivers/bus_mailbox/vcmailbox/Run`.
  `cargo xtask image` cross-compiles the driver position-independent for
  `aarch64-unknown-none` (its own `Run.ld`), converts the linked PIE ELF to an
  `rxe` relocated for the production user-image bias and stamped with the
  kernel's `SYSCALL_TABLE_HASH`, and wraps it as a `kind = UserSpace`
  `DriverManifest` requesting `CAP_MMIO_MAP` + `CAP_MEM_DMA` +
  `CAP_IPC_BIND_PRIVILEGED`, signed with the kernel's driver-signing seed — so
  the booted kernel admits it through the §8 / §18.6 signed load gate. The
  autoload gate's delegatable superset (`unlock_service::autoload_caps`) carries
  `CAP_IPC_BIND_PRIVILEGED` precisely so a signed bus *service* driver like this
  one can be granted the privilege to bind its restricted-sender endpoint; the
  per-driver manifest∩superset intersection still binds, so a driver that does
  not request it receives nothing extra (`AGENTS.md` §5.2 / §18.3). The
  ELF→`rxe` converter and signer are the shared definitions the kernel
  `build.rs` and the autoload fixtures also use (`AGENTS.md` §2.2);
  `tools/mkimage` only plants the bytes (`build_rpi_image`'s `drivers`
  argument), it never drives `cargo`. The store-planting routine is the single
  `rustos_drv_fs_rustfs::plant_nested_file`.

Metal-only (`plans/PI.md` §0.4): QEMU `virt` models no `VideoCore` mailbox, so
the wire protocol, the client channel, and the server transform are host-tested
(`lib/abi`, `lib/drvrt`, `lib/vcmailbox`), and the install is host-tested
(`tools/mkimage` plants and reads the bundle back from the read-only `/System`
store; the image builds end to end in the CI image gate). `devmgr` autoloading
the bundle against the real discovered BCM2711 mailbox node, and the user-space
USB-keyboard chain below, are verified on hardware (`plans/PI.md` §0.9).

## USB-keyboard chain (video-console keyboard backing, P10 D5d)

The video console's read half is fed by a directly attached USB keyboard
on the Pi 4: the VL805 xHCI controller behind the BCM2711 PCIe root
complex. That chain comes up **entirely in user space** as autoloaded
signed `/System/Drivers/` bundles — there is **no** in-kernel keyboard
service (the former scaffold was deleted at `plans/PI.md` P10 D5d, §2.14).
The boot path's only role is discovery and Device-typing the windows:

- **Discovery.** `platform::FdtDiscovery` emits the `brcm,bcm2711-pcie`
  root complex into the hardware tree carrying its three windows as
  capability-grant *requests* — the controller `reg` (`Mmio`), the inbound
  `dma-ranges` aperture (`Dma`), and the outbound `ranges` window
  (`BusWindow`) — and the VideoCore mailbox node carrying its DMA property
  buffer. A tree with no such node (the QEMU `virt` shape) emits neither and
  the chain simply never autoloads (§18.4).
- **Identity Device mapping.** `boot_aarch64` folds the discovered
  controller-register and outbound-MMIO-window gigapages into the identity
  `Device` mask (`identity_device_mask`) **before** enabling the MMU, so
  those windows are Device-typed once translation is on (the user-space
  driver maps them into its own address space through `mmio_map`).
- **User-space autoload.** `devmgr` autoloads each signed bundle against its
  discovered node: `pcie_brcm` binds the bridge, trains the link, assigns
  the VL805 BAR, and emits the VL805 PCI function (`hw_emit_node`); `vl805`
  binds that, reloads the controller firmware over the VideoCore mailbox,
  and emits the `usb,xhci` node; the `xhci` HCD binds that, maps the BAR,
  carves DMA, brings the controller up, enumerates every reachable device,
  and emits one interface node per served device; each class driver
  (`usb_kbd` for the boot keyboard, `usb_msd` for a storage stick) binds its
  interface node and drives its device over the URB transport — the
  keyboard pumping decoded key edges into the input-focus arbiter through
  `key_inject`. Each
  driver is granted only the resources its matched node requested (§18.3),
  reached through its rt-backed `DriverHost`. The bridge→CPU BAR translation
  is resolved in user space (`pcie_brcm` emits the BAR as a CPU-physical
  `Mmio` grant inside the bridge's outbound window), while the **inbound-DMA
  translation** — turning a carved CPU-physical buffer into the device-visible
  bus address the bridge maps back (the Pi 4 `IB MEM 0x0..0x1ffffffff ->
  0x4_0000_0000` viewport) — is resolved kernel-side in `dma_alloc`
  (`devres::translate_device_addr`), since only the kernel knows the carve's
  physical base. The bridge forwards its inbound aperture verbatim onto the
  VL805 node, so the grant the xHCI driver receives carries the same
  translation the bridge holds (`AGENTS.md` §18.1).

QEMU models no Pi USB (§0.4), so the engines are host-tested up to the
controller hand-off and the live enumerate→emit→autoload chain (a real BAR,
link training, a keyboard driving the login) is the on-metal acceptance
item (§0.9).

> **Note (`plans/PI.md` P10 D5d):** the per-register PCIe/xHCI/firmware
> diagnostics the chronicle below references (events in the `4101`–`4126`
> range) were emitted by the now-deleted in-kernel scaffold. They are gone
> from the live kernel path; the chronicle is retained only for the **PCIe
> root-cause findings** that still apply to `drivers/bus/pcie_brcm` and the
> user-space bring-up.

### Discovery and bring-up logging (metal diagnostics)

Because the live bring-up is metal-only, it logs its progress to the
serial sink so a silent keyboard can be diagnosed from a UART capture
alone (the boot beacons above bound the *boot* path; these events bound
the *USB* path). All are allocation-free (§2.9). The bring-up events are
one-shot; the three poll-loop events (`4129`/`4130`/`4131`) run *on* the
forever poll loop but are **bounded** (a one-shot first report, an
on-change capped error, and a capped heartbeat), so the log stays finite
(§2.16 / §19.4):

| Event id | Emitted by | Says |
| -------- | ---------- | ---- |
| `4100` | `boot_aarch64` (post-MMU, beacon `4a`) | the discovered `brcm,bcm2711-pcie` chipset windows — `regs_base/len`, the inbound aperture (`dma_aperture_top`, `inbound_size`, `inbound_pcie_base`), and the outbound window (`outbound_cpu_base/pcie_base/size`) — so a capture shows the hardware the chain will program. Absent on `virt` (no bridge). |
| `4103` | `keyboard_service::bring_up_keyboard_into_tree` / `spawn_pump` | the report-pump kthread was admitted, or the bring-up was skipped because no kernel frame allocator was available (an error). Silent on `virt`. |
| `4101` | `usb_keyboard::bring_up_keyboard` | each bring-up stage: link-training start, root-complex link trained, xHCI online, and — at `Error` level with an `err=` field — the stage that refused (PCIe link, xHCI, or root-hub enumeration). An xHCI open failure also carries `stage` (`capability`, `halted_before_reset`, `reset_self_clear`, or `controller_ready_after_reset`) plus `usbcmd_hex`/`usbsts_hex`, so a valid capability block followed by `device_fault` is localised to the exact stuck reset condition. |
| `4105` | `keyboard_service::IdentityMmioMapper` | one map-window decision: `phys_base_hex`/`len_hex` (the address the PCI driver asked the bridge to map — for the BAR, the value the VL805's BAR register holds), `resolved_cpu_hex` (the backed CPU address, or the `ffff_ffff_ffff_ffff` sentinel when refused), and the accepted-window bounds (`regs_base/end`, `outbound_pcie_base/end`). Logged at `Error` when refused. Two lines on a healthy bring-up: the controller regs block (identity) and the VL805 BAR (bus→CPU translated). |
| `4108` | `usb_keyboard::bring_up_keyboard` | the outcome of the per-boot VL805 firmware reload (`NOTIFY_XHCI_RESET`), issued once after the link trains (its `PERST#` drops the VL805's `VideoCore`-loaded firmware on EEPROM-less Pi 4 boards): `skipped because no videocore mailbox is available` (`NotAvailable`), the honoured reload (`Reloaded`), or — at `Error` level — `reload via the videocore mailbox failed reason=<window\|timeout\|firmware_error\|malformed_response\|bad_aperture\|bad_geometry\|unknown>`. The VL805 device driver (`drivers/bus/usb/vl805`) runs the reload over `DriverHost::mailbox` from inside the floor xHCI bring-up; best-effort, the authoritative liveness gate is `Xhci::open`. A `reason=timeout` is expanded by the `4121` record, which says *where* the mailbox exchange timed out. |
| `4120` | `usb_keyboard::bring_up_keyboard` | a one-shot capture of the controller's **inbound** (PCIe→system-memory) viewport registers **as the previous boot stage (`start4.elf`) left them**, sampled before bring-up programs `RC_BAR2` (`BrcmPcieRc::entry_inbound_window`): `rc_bar1_lo_hex`/`rc_bar3_lo_hex` (the unused PCIe→GISB / PCIe→SCB inbound windows), `rc_bar2_lo_hex`/`rc_bar2_hi_hex` (the active PCIe→system-memory viewport — offset bits plus the encoded size in the low field), `misc_ctrl_hex` (the inbound-path `MISC_MISC_CTRL`, whose `SCB0_SIZE` field in bits `[31:27]` sizes the inbound SCB→memory decode window), and `pcie_status_hex` for correlation. raspberrypi/firmware #1495: `VideoCore`'s `NOTIFY_XHCI_RESET` firmware load *assumes* the `RC_BAR2` state it set at power-on, so this capture both drives bring-up's "preserve a firmware-configured `RC_BAR2`" decision and lets a metal run compare the firmware's own inbound window against the known-good `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`. A faulting read renders the all-ones sentinel; always `Info`. |
| `4121` | `keyboard_service::KernelMailboxChannel` | one-shot diagnostics from each VL805 firmware-reload mailbox exchange (`MmioMailbox::last_exchange_stats`), logged by the channel after every `exchange` whether it succeeded or failed: `timeout_stage` (`post_room` = the firmware never accepted the request; `response` = it accepted but never replied; `none` = no timeout), `posted_word_hex`, `post_room_polls_hex`/`response_reads_hex`, `foreign_channel_reads_hex`, `last_status_hex`, plus `wait_elapsed_us_hex` (the `CNTPCT_EL0`-measured wall time the exchange took) and `poll_budget_hex` (`FIRMWARE_RELOAD_POLL_BUDGET`). A bare `4108 reason=timeout` cannot tell a transport fault from `VideoCore` dropping the tag; this localises it. Always `Info`. |
| `4116` | `keyboard_service::bring_up_keyboard_into_tree` | a one-shot **bring-up delay timing measurement**, logged once right after the VL805 bring-up chain returns. `requested_us_hex` is the total the code *asked* its `GenericTimerDelay` to wait across the whole chain (over `delay_calls_hex` calls); `counter_elapsed_us_hex` is the same span measured by `CNTPCT_EL0` against `CNTFRQ_EL0` (also echoed as `timer_hz_hex`). The metal capture read `requested_us_hex=0x57030` (≈356 ms / `0x103`=259 calls) yet `counter_elapsed_us_hex≈14.3 s` at the correct `timer_hz_hex=0x337_f980` — so ≈14 s of *real* time elapsed with only ≈356 ms of it in `busy_delay_us`: the counter is sound, the seconds are code-side. `4116` cannot split *where* in the chain they go; the per-line `[t=<ms>ms]` timestamps and `4117` do. The earlier guess that the ≈14 s was the 256 caps-readiness polls (`4109`) was **wrong**: a timestamped capture showed the caps wait is only ~0.35 s (the wall-time `wait_for_caps_ready` bound works; the master-abort returns the poison fast, not ~54 ms) and ~11 s of the pause is inside `BrcmPcieRc::bring_up` (`4117`). Always `Info`. |
| `4117` | `usb_keyboard::bring_up_keyboard` | a one-shot per-phase wall-time split of the PCIe root-complex `bring_up`, logged right after the link-trained line: `reset_swinit_us_hex` (releasing the always-accessible `RGR1_SW_INIT_1` `0x9210` bridge `sw_init` reset the previous boot stage left asserted, run **first**; `train_link` deasserts the already-asserted `PERST#`, and that deassert edge re-triggers the `VideoCore` VL805 firmware reload), `reset_settle_us_hex` (the post-de-reset MISC settle — the gentlest no-touch-probe bring-up does **not** toggle the SerDes IDDQ or re-assert a fundamental reset, either of which could drop the resident VL805 firmware), `config_us_hex`, `linkwait_us_hex`, `link_polls_hex`, and `entry_rgr1_sw_init_hex`. The BCM2711 holds the controller core off until the RGR1 bridge `sw_init` reset is cycled, so the bring-up releases that reset **before** any MISC access (matching the BCM2711 PCIe bring-up sequence); the metal capture confirmed `reset_swinit_us`/`reset_settle_us` collapse to microseconds (the ~11 s pause is gone). `entry_rgr1_sw_init_hex` is the raw `RGR1_SW_INIT_1` register sampled at bring-up entry **before** the reset cycles it (the always-accessible RGR1 block needs no link/MISC). The metal capture read `0x3` (both `PERST#` bit 0 and the bridge `sw_init` bit set), i.e. the previous boot stage handed off with PCIe held in fundamental reset — RustOS never writes this register outside its own reset, so it is the firmware handoff state, not something RustOS asserted, and is the same cold-reset state the BCM2711 bring-up handles (so **not** itself the fault). The persistent `dead_dead`/`vl805_fw_version_hex=0` is instead explained by the root-port bridge command not latching Memory Space Enable when written pre-link (see the bridge-command section) — which blocks both our BAR reads and `VideoCore`'s firmware-load writes over the same bus — now enabled after link-up. The `*_us` spans sum to the whole bring-up; `BringUpTiming` and both the release-before-MISC ordering and the no-re-assert / `PERST#`-deassert-edge invariant are host-tested in `rustos_pcie_brcm` (`AGENTS.md` §15.7). Always `Info`. |
| `4129` | `usb_keyboard::KeyboardPumpDiagnostics` | **one-shot**, the first time the poll loop drains a non-zero event count after bring-up: the addressed keyboard's interrupt-IN endpoint is actually completing transfers and decoded edges are reaching the input arbiter. Carries `polls_hex`/`events_hex`. Its *absence* in a capture while `4131` keeps climbing localises a silent keyboard to "addressed but the controller never completes the interrupt endpoint", distinct from `4130`. `Info`. |
| `4130` | `usb_keyboard::KeyboardPumpDiagnostics` | the poll loop's `pump_once` returned an error, logged when the error *kind* changes (capped at `MAX_ERROR_LOGS` = 16 so a wedged controller faulting every poll cannot flood the log). Carries the `err` name (e.g. `device_fault` — an unexpected event type or a slot/endpoint mismatch in `UsbDevice::next_report`), `polls_hex`, and `errors_hex`, so a capture names *why* the report path faults rather than the loop silently swallowing it. `Error`. |
| `4131` | `usb_keyboard::KeyboardPumpDiagnostics` | a periodic liveness heartbeat of the keyboard poll loop, emitted every `HEARTBEAT_POLLS` (1024) polls and capped at `MAX_HEARTBEATS` (32) total so the log is finite though the loop runs forever. Carries cumulative `polls_hex`/`events_hex`/`errors_hex`: a capture where polls climb while events and errors stay zero proves the loop is alive and polling but the keyboard delivers no reports — the exact signal a "typing produces nothing" symptom needs. `Info`. |

The last `4101` line a capture shows pins which stage a silent keyboard
stalled at; the absence of `4102` after an `xHCI online` `4101` means the
root hub bring-up failed, and the `4125` records then show every root-hub
port's `PORTSC` (whether power stuck and whether any port reports a device
attached) while the single `4126` record names the enumeration step the
bring-up last entered and the xHCI completion code it saw there — together
they distinguish an empty hub (`err=not_found`, `4126 stage=0`) from a
device that is present but faults part-way through enumeration.

**Enumeration orchestration.** The arch-neutral root→hub→downstream
bring-up sequence (enumerate the first connected root-hub port; if it is a
hub, power its ports and address the device behind **every** connected one
on its own xHCI slot, up to `MAX_DEVICES` at once) lives in one place —
`rustos_usb::device::UsbDevice::bring_up` — so every device reached through
the Pi 4B's onboard hub is *discovered*, never a guessed port (`AGENTS.md`
§2.2 / §18): a keyboard and a storage stick plugged in together are both
served, neither displacing the other. The HCD calls it once at bring-up; a
device absent at that point is not a failure — the controller comes up with
the first-connect watch armed (the onboard hub's status-change endpoint, or
the root port), so a cold boot with nothing plugged in works and each device
autoloads when plugged in. A real enumeration fault on one port skips that
port fail-closed without costing the others their service.

**Keyboard poll-loop diagnostics (`4129`/`4130`/`4131`).** Once the
keyboard is brought up, the keyboard
service polls it forever (`pump_once` → decode → `ArbiterConsoleSink`).
That loop historically discarded its result, so a capture where the
keyboard was addressed yet typing produced nothing could not say whether
reports were arriving, whether `next_report` was faulting, or whether the
loop was even running. `KeyboardPumpDiagnostics` folds each poll result
into bounded audit events: a one-shot `4129` the first time a report
drains (keystrokes are flowing), an on-change `4130` carrying the
`DriverError` name when `pump_once` faults (capped at 16), and a capped
`4131` heartbeat (every 1024 polls, ≤ 32 total) carrying cumulative
`polls`/`events`/`errors`. The three readings split the failure cleanly:
`4129` present ⇒ the path works; `4130 err=device_fault` recurring ⇒
`next_report` rejects the controller's interrupt-IN events (a slot/EP or
event-type mismatch); `4131` polls climbing with `events=0 errors=0` ⇒
the loop is alive but the controller never completes the interrupt
endpoint (the addressed keyboard delivers no report). Host-proven by the
`usb_keyboard` tests `pump_diagnostics_logs_the_first_report_only_once`,
`pump_diagnostics_logs_a_pump_error_on_change_and_caps_it`, and
`pump_diagnostics_emits_a_bounded_heartbeat`; metal-only beyond that
(§0.4).

**Interrupt-endpoint Max ESIT Payload — the no-report fix.** The metal
capture then read exactly the `4131` branch: the heartbeat climbed
(`polls` `0x400` → `0x8000`) with `events=0 errors=0`, no `4129` and no
`4130`. The loop was alive and polling, `next_report` never faulted, yet
the addressed full-speed keyboard delivered no interrupt report. Root
cause: the interrupt-IN endpoint context (`ep_ctx_dwords`) left **Max
ESIT Payload** zero (§6.2.3.8 dword 4 bits 16:31). The xHCI periodic
scheduler reserves no bus bandwidth for a periodic endpoint whose Max
ESIT Payload is zero (§4.14.2), so the controller scheduled the
keyboard's split transactions through the hub's TT *never* — Address
Device and Configure Endpoint both succeed (hence `4128`), but the
endpoint is serviced never. The fix programs Max ESIT Payload = the max
packet size for any periodic (non-zero-Interval) endpoint; a control
endpoint leaves the field reserved-zero. Host-proven by
`the_downstream_interrupt_endpoint_carries_a_nonzero_max_esit_payload`
(and the existing report-drain tests, which the faithful mock now gates
on a non-zero payload — all fail before the fix and pass after);
metal-only beyond that (§0.4).

**Interrupt endpoint read from the descriptor — the no-report fix that
held.** The Max ESIT Payload fix did not change the metal symptom: the
re-flash again read the `4131` branch, heartbeat climbing
(`polls 0x400` → `0x8000`) with `events=0 errors=0`, no `4129`/`4130`,
the addressed Keychron (`3434:0e21`, slot 2) silent. The remaining
assumption was the *endpoint itself*: the driver hard-coded the
interrupt-IN endpoint as endpoint 1 (DCI 3, with a fixed interval and
8-byte packet) and never read the keyboard's endpoint descriptor. A
keyboard whose interrupt-IN endpoint is not endpoint 1 then has its
Configure Endpoint, doorbell, and `next_report` all aimed at the wrong
DCI, so the controller schedules the real endpoint never — Address
Device and Configure Endpoint succeed (hence `4128`, the hub marked, a
non-zero Max ESIT Payload), yet no report flows. `InterfaceInfo::decode`
now walks past the matched interface to its first interrupt-IN endpoint
and captures its DCI (`2 × endpoint_number + 1`), `wMaxPacketSize`, and
`bInterval`; `finish_enumeration` configures/doorbells/drains that DCI
(stored as `UsbDevice::int_dci`), and `interrupt_interval` derives the
endpoint-context Interval from `bInterval` and the device speed (xHCI
Table 6-12) instead of a fixed exponent. A HID interface that reports no
interrupt-IN endpoint fails closed (`BadMagic`, §2.9). Host-proven by
`downstream_keyboard_is_serviced_on_its_descriptor_reported_endpoint` (a
mock keyboard whose interrupt endpoint is endpoint 2 → DCI 5: the
Configure Endpoint must name DCI 5 and the report drains on it — fails
before the fix, passes after). **Metal: confirmed** — the re-flash drove
the on-screen `Username:`/`Password:` prompt from the USB keyboard
(`4129` drained the first report, `4131` then climbed with the keystroke
count, `errors=0`), completing the Pi 4B USB-HID keyboard path
end-to-end.

**Root-hub port power (`PORTSC.PP`) — the post-firmware lever.** Once the
inbound `SCB0_SIZE` fix let `VideoCore` land the VL805 firmware blob, the
metal capture advanced decisively: `4118 fw_version=0x138c0 ready=1`,
`4108` *reloaded*, `4123 firmware_loaded=1`, and `Xhci::open` brought the
controller fully **online** (`4101 … enumerating root hub`, `4106`
`max_ports=5`). The residual moved to root-hub enumeration: `4101 no usb
device enumerated on the root hub err=device_fault`. The cause was that
the scan read each port's Current Connect Status **once**, immediately
after Run, with no Port Power asserted and no connect debounce — but the
Host Controller Reset in `Xhci::open` clears every `PORTSC`, and the VL805
is port-power-controlled (`HCCPARAMS1` PPC = 1), so a powered-off port
reports disconnected no matter what is plugged in (xHCI 1.2 §4.19.1.1).
`UsbDevice::enumerate_first_connected` now asserts `PORTSC.PP`
(`Xhci::set_port_power`, masking the write-1-to-clear bits) on every
reported port, then debounce-polls `1..=max_ports` (bounded by the
engine's budget, fail-closed `NotFound` on a genuinely empty hub) for the
first port to report a device. The `4125` per-port diagnostic captures
the post-power `PORTSC` for the next metal run. Host-proven by the
`lib/usb` tests `set_port_power_asserts_pp_and_rejects_a_bad_port`,
`enumerate_first_connected_powers_every_root_port`, and
`enumerate_first_connected_connects_a_port_only_after_power` (a mock
modelling a device that reports connected only once its port is powered).
Metal-only beyond that (QEMU models no Pi PCIe/USB, §0.4).

**Enumeration fault localisation (`EnumStage` / `4126`).** The port-power
fix put a connected device on the root hub (`4125` port 1
`ccs=1 pp=1 ped=1 speed=3`) with the other four ports powered but empty,
yet `UsbDevice::enumerate_first_connected` still returned `device_fault`
*inside* `enumerate_hid`. The single coarse `DriverError::DeviceFault`
could not pin that down, so the driver keeps a breadcrumb:
`UsbDevice::enum_stage` records the `EnumStage` it last entered, and
`UsbDevice::last_completion_code` the raw xHCI completion code of the last
event that step observed (reset to `0` — "none/timeout" — at the start of
each command/control transfer). On the failure path `bring_up_keyboard`
logs both as `4126` (`stage_hex` + `completion_hex`) before the `4125`
port dump. Host-proven by the `lib/usb` tests
`enumerate_hid_records_the_configured_stage_on_success` and
`enumerate_hid_stage_breadcrumb_localises_a_class_stall`.

**Non-coherent DMA cache maintenance.** The metal `4126` capture read
`stage_hex=2` (Enable Slot) with `completion_hex=0`: the *very first*
command issued to the online controller never produced a Command
Completion event. The controller is alive (its capability block reads
live over MMIO) and a device is attached, so the controller is not
processing the command ring at all. One contributor is a **cache
coherency** gap: the BCM2711 PCIe root complex is **not** I/O-coherent
(it does not snoop the CPU caches — this is why the VideoCore mailbox
buffer and the HVS framebuffer already perform explicit cache
maintenance). The user-space driver maps its device-shared DMA slab
Normal-Non-Cacheable, but the kernel zeroes each allocated/freeing carve
through the cacheable direct-map alias; those dirty zero cache lines must
be cleaned and invalidated before the controller or a later owner uses the
same frames. The production `PhysMap` therefore exposes a
`clean_invalidate(phys, len)` hook; `DmaPool` calls it after zeroing on
allocation and free, and the aarch64 configured identity map routes it to
`clean_invalidate_dcache_range` (`dc civac` + `dsb`). Coherent ports and
host tests keep the no-op default. Host-proven by
`alloc_cleans_direct_map_alias_after_zeroing` and
`free_cleans_direct_map_alias_after_zeroing` in `kernel/mem`, plus the
existing `DmaSlab` coherency tests for user-space slab read/write
bracketing. Necessary, but on its own **not sufficient**: a rebuilt image
carrying the earlier user-slab maintenance still captured `4126
stage_hex=2 completion_hex=0`, so the controller had a second reason not
to consume the command ring.

**Scratchpad buffers.** That residual was the missing
xHCI **scratchpad buffers**. `HCSPARAMS2` carries a *Max Scratchpad
Buffers* field: page-sized buffers system software must reserve and point
`DCBAA[0]` at before the controller can run (xHCI §4.20). The VL805
datasheet reports `HCSPARAMS2 = 0xFC00_0031` — **31** scratchpad buffers
(`SPR = 1`) — but the driver read neither `HCSPARAMS2` nor `PAGESIZE` and
allocated none, so `DCBAA[0]` stayed zero and the controller had nowhere
to save state: it accepts Run/Stop and reports live capability registers,
yet executes no command, producing exactly the `stage=2 completion=0`
signature. `Xhci::open` now reads `HCSPARAMS2`/`PAGESIZE` (exposed as
`max_scratchpad_buffers()` / `page_size()`, surfaced in the `4106`
geometry line as `max_scratchpad_hex`), `device::Layout` reserves a
page-aligned scratchpad pointer array plus that many page-aligned buffers
(failing closed if the carve cannot hold them or a scratchpad-requiring
controller reports no page size / an unaligned base), and
`UsbDevice::start` fills the array with each buffer's device-visible base
and points `DCBAA[0]` at it. The device-shared DMA carve
(`wiring::XHCI_DMA_BYTES`) grew from 16 KiB to **256 KiB** to hold 31 ×
4 KiB scratchpad pages plus the rings/contexts. Host-proven by
`start_reserves_scratchpad_and_programs_dcbaa0` (a mock that, like the
VL805, withholds every command completion until `DCBAA[0]` is programmed —
enumeration then runs end to end), `start_stalls_without_scratchpad_on_a_controller_that_needs_it`
(fail-closed when the region is too small), and the
`hcsparams2_decodes_the_vl805_scratchpad_count` / `pagesize_decodes_the_lowest_supported_page`
register-decode tests. **Metal-confirmed:** the capture read `4106
max_scratchpad_hex=0x1f` (the count is read) and `4126` advanced from
`stage_hex=2` all the way to `stage_hex=8 completion_hex=6` — the command
ring now runs and the device addresses, reads its descriptors, and
configures; the missing scratchpad was the blocker.

**`SET_PROTOCOL(boot)` STALL tolerated.** With the
scratchpad reserved, enumeration reached the *final* step and stopped
there: `4126 stage_hex=8 completion_hex=6` — the HID `SET_PROTOCOL(boot)`
class request (`EnumStage::SetProtocol`) answered **STALL** (completion
code `6`). `SET_PROTOCOL` is mandatory only for boot-subclass devices
(HID 1.11 §7.2.6); a device that does not implement it STALLs the request,
and that is a *protocol* stall — per USB 2.0 §8.5.3.4 the default control
endpoint resumes on the next SETUP, so the device stays usable in its
default protocol (this is exactly what USB HID class drivers do: a stalled
`SET_PROTOCOL` is ignored). The driver previously treated *any* non-Success
control completion as a `device_fault`, so the optional request's STALL
aborted an otherwise fully enumerable keyboard. `enumerate_hid` now issues
`SET_PROTOCOL(boot)` through a new `control_optional` helper that absorbs a
STALL completion (the raw code is still preserved in `last_completion`),
continues to prime the interrupt-IN ring, and reaches `EnumStage::Configured`;
every *other* completion still fails closed (`AGENTS.md` §2.9 / §5.4). It
is issued as the last EP0 transfer of enumeration and EP0 is not used
again, so a halted control endpoint after the STALL is immaterial.
Host-proven by `enumerate_hid_tolerates_a_stalled_set_protocol` (a mock
that STALLs the class request now enumerates to `Configured` with
`last_completion = Stall` and no protocol selected) and
`enumerate_hid_fails_closed_on_a_non_stall_class_fault` (a genuine
non-STALL class fault still aborts at the `SetProtocol` breadcrumb).
**Metal-confirmed:** the capture now logs `4102 vendor=2109 product=3431`
— enumeration runs end to end. The tolerated STALL *was* the last
enumeration blocker; what enumerates, however, is not the keyboard (see
the hub-topology lever below).

**Hub topology — the current lever.** The `4102` device is `2109:3431` —
the Pi 4B's **onboard VIA Labs USB hub**, which sits between the VL805
root hub and the four USB-A ports. The keyboard is plugged into a USB-A
port, so it enumerates *downstream* of that hub, not on a root-hub port;
the bring-up enumerated and configured the hub itself, but a hub
delivers no HID reports, so login still sees no keystrokes. The device's
`bDeviceClass` is `0x09` (`DeviceDescriptor::is_hub`), so reaching the
keyboard requires walking the hub. When the enumerated device is a hub the
bring-up reads its `bNbrPorts` (class `GET_DESCRIPTOR(hub)`), asserts Port
Power on every downstream port (class `SET_FEATURE(PORT_POWER)`), waits the
power-on-good window, and logs each downstream port's hub-class
`GET_STATUS` as `4127` (`UsbDevice::hub_num_ports` / `power_hub_port` /
`hub_port_status`, issued over the hub's already-addressed default control
endpoint). Each `4127` record carries `completion_hex` (the per-port
`GET_STATUS` transfer's raw xHCI completion code) so the record whose
`connected_hex=1` pins which downstream port the keyboard is on and at
what `speed_hex` — *provided the read succeeded* (`completion_hex=1`); a
faulting read instead renders the all-ones `wstatus` sentinel and the
`completion_hex` says why.

**EP0 halted by `SET_PROTOCOL` on the hub — fixed.** The metal capture
then read `4101 reading the hub descriptor failed err=device_fault`: the
class `GET_DESCRIPTOR(hub)` faulted even though the device-descriptor and
configuration-descriptor reads on the same EP0 had succeeded. Root cause:
`enumerate_hid` issued the HID `SET_PROTOCOL(boot)` to *every* enumerated
device, including the hub. A hub is not a HID device, so it STALLs that
class request — and an xHCI STALL **halts** the control endpoint (xHCI
§4.10.2.4): the controller runs no further TRBs on EP0 until software
resets it. `control_optional` *tolerated* the STALL (its safety rested on
"the request is the last EP0 transfer of enumeration"), but that invariant
breaks for a hub, whose bring-up reuses EP0 for the hub-descriptor read —
which then ran on a halted endpoint and faulted. Fixed by issuing
`SET_PROTOCOL(boot)` **only** to a HID interface
(`InterfaceInfo::is_hid`, `bInterfaceClass == 0x03`); a hub's interface
(class `0x09`) never receives it, so its EP0 is never STALL-halted and the
hub-descriptor read runs cleanly. A keyboard plugged directly into the
root hub is unaffected — its HID interface still gets `SET_PROTOCOL(boot)`,
and it remains the last EP0 transfer. The `4127` "reading the hub
descriptor failed" log now also carries `completion_hex` (the failed
transfer's raw xHCI completion code) so a future fault distinguishes a
STALL (`6`) from a transaction error (`4`) or a missing completion (`0`).
Host-proven by the `lib/usb` tests
`enumerate_hid_flags_a_hub_via_the_device_class`,
`enumerating_a_hub_leaves_ep0_usable_for_the_hub_descriptor` (the mock now
STALLs the hub's `SET_PROTOCOL` and models the EP0 halt, so it fails before
the gating fix and passes after),
`hub_discovery_finds_the_downstream_device`,
`hub_port_reads_disconnected_until_powered`, and
`hub_num_ports_fails_closed_on_a_forged_descriptor`.

**Per-port `GET_STATUS` faults — the current lever.** With the EP0 fix the
hub-descriptor read succeeds (`4127 num_ports=4`) and Port Power is
asserted on every downstream port, but the metal capture then read every
port's `wstatus_hex=0xffff` (the all-ones sentinel) with
`completion_hex=0` — each per-port class `GET_STATUS` (USB 2.0 §11.24.2.7)
faulted (`hub_port_status` failed closed and the loop rendered the
sentinel). The sentinel-decoded `connected=1 speed=2` are therefore
artifacts, not real reads, so we still cannot tell which downstream port
holds the keyboard.

The `completion_hex=0` was **not** the timeout it appeared to be: the four
`4127` records are spaced at the same ~250 ms serial-logging cadence as
the non-faulting `4125` lines, i.e. each `GET_STATUS` failed *fast*, not
after spending the million-iteration poll budget — so an event almost
certainly *did* arrive. The fault was a diagnostic gap.
`UsbDevice::control`/`command` recorded `last_completion` only *after*
`await_event_for` returned `Ok`, but `await_event_for` returns `Err`
**before that** when the event it observes is for an unexpected TRB
address *or* carries a completion code the driver's `CompletionCode` enum
does not model (its fail-closed `completion_code()` decode rejects xHCI
codes outside {1,2,3,4,5,6,13}). Those paths left `last_completion` at the
`0` "no event" sentinel, so a real-but-rejected code was mislabelled as a
timeout. `await_event_for` now records
`self.last_completion = event.completion_code_raw()` the moment it
observes any command/transfer event — before the address match and before
the decode — so `completion_hex` is truthful: a genuine `0` now means no
event was seen at all, while a non-zero value (including a reserved /
controller-specific code) names what the hub actually answered
(`AGENTS.md` §15.7). The now-redundant post-`await` assignments in
`control`/`command` were dropped (§2.2/§2.14). Host-proven by
`faulting_hub_port_status_records_the_completion_code` (a STALLed
`GET_STATUS`, code `6`) and the new
`faulting_hub_port_status_records_an_undecodable_completion_code` (a
`GET_STATUS` answered with the unmodelled xHCI code `7`, Resource Error:
the read fails closed on the decode yet `last_completion_code()` now
retains `7` — it read `0` before the fix).

With `completion_hex` truthful, the next metal capture named the fault:
ports 1–2 read `completion_hex=0x0d` (xHCI ShortPacket, the IN data
stage) and ports 3–4 `completion_hex=0`, **all** still failing closed
(`wstatus=0xffff`). Every *other* EP0 control transfer — the device,
configuration, and hub descriptors, `SET_ADDRESS`/`SET_CONFIGURATION`,
and `SET_FEATURE(PORT_POWER)` — succeeds on this same EP0, so only the
class `GET_STATUS` fails. `control` already tolerates a ShortPacket data
stage (it is expected for a variable-length IN), so the `0x0d` is the
*data* stage being recorded, then the **status-stage** event never
satisfying the wait: `await_event_for` returns `Err` *fast* (the
~250 ms logging cadence, not the million-iteration budget), so a real
event arrives that the wait rejects — not a timeout.

**Localising the reject — the current lever.** The remaining gap was
that `await_event_for` discarded *why* it rejected (an unexpected TRB
type vs. a TRB-address mismatch vs. an undecodable completion code vs. a
true budget timeout) and *what* event it saw. It now records both: a
`last_reject` reason (`1` unexpected type, `2` address mismatch, `3`
undecodable code, `4` budget timeout) and the rejected event's raw
TRB-type (`last_event_type`), reset per transfer alongside
`last_completion` and exposed via `UsbDevice::last_reject_reason` /
`last_event_type`. Behaviour is unchanged (the same fail-closed `Err`,
`AGENTS.md` §2.9); only the diagnostic is richer. Each `4127` record now
carries `evtype_hex` + `reject_hex` so the next capture pins which:
`reject_hex=1` with a non-`0x20` `evtype_hex` is an asynchronous
controller event reaching the status-stage wait (the likely cause of the
fast reject), `reject_hex=4` would be a genuine timeout. Host-proven by
`faulting_hub_port_status_records_an_unexpected_event_type` (a
`GET_STATUS` answered by a `NoOp`-type event: the wait fails closed,
`last_reject_reason()=1`, `last_event_type()=8`, and
`last_completion_code()` stays a truthful `0`).

**Still to do (the next lever):** once a port reports a real
`connected_hex=1`/`speed_hex`, actually *addressing* the downstream
device needs a second xHCI slot whose slot context carries the **Route
String** (the downstream hub port) and, for a low/full-speed device
behind this high-speed hub, the **TT** hub-slot/port fields — a larger
change deferred until that capture. Metal-only beyond the host tests.

The `4104` scan first ran with only **one** function — the root-complex
bridge itself (`14e4:2711`, class `0604`) at BDF 0 — and no `1106:3483`
VL805 downstream. That localised the failure to PCIe *discovery*: the
BCM2711 ships its root port's type-1 bridge bus-number register
(`PCI_PRIMARY_BUS`, config offset `0x18`) at 0, so the port forwarded no
configuration transactions to the secondary bus and the VL805 on bus 1
never answered a read. The root-complex bring-up
(`rustos_drv_bus_pcie_brcm::BrcmPcieRc::bring_up`) now programs that
register (primary 0, secondary 1) so configuration reaches bus 1.

Enabling that forwarding, however, exposed a second defect that **wedged
the boot** (the capture stops right after `4101 pcie root-complex link
trained`, before any `4104` line): the bus walk is a *flat* scan over all
256 buses, and once the root port forwards downstream, a config read to a
target that does not exist — any device other than `01:00.0`, or any bus
beyond the directly-attached one — forwards a configuration TLP onto the
link that nothing answers. The root port's `CFG_READ_UR_MODE` only
master-aborts a request the RC itself can refuse; a *forwarded* TLP
instead waits for a completion that never arrives, and the timeout
manifests as a CPU external abort that hangs the boot CPU. The fix is in
the windowed configuration accessor
(`rustos_pci::mechanism_brcm`): the BCM2711 root port is a
single-device link, so the accessor now forwards a configuration
transaction **only** to `device 0` on the secondary bus and resolves
every other downstream target to the PCI "no device" sentinel *without*
issuing a transaction (it never forwards to an absent target). The bridge
subordinate is likewise kept equal to the secondary bus (no on-board
switch to reach). With that fix the `4104` scan lists **two** functions —
the bridge (`14e4:2711`, class `0604`) and the VL805 (`1106:3483`, class
`0c03`) — confirming discovery is complete.

Discovery then handed off to a third defect at the **xHCI controller
bring-up** (`4101` reported `err=out_of_range`, right after the two-function
`4104` scan): the device-shared DMA carve was bounded in the wrong address
space. `DmaSlab::phys()` is a *device-visible* (PCIe-space) address, but
both the kernel DMA host (`keyboard_service::FrameDmaHost`) and the USB
wiring (`rustos_drv_bus_usb::wiring::open_discovered`) compared it against
the *CPU-physical* inbound-aperture top (`dma_aperture_top` =
`0x2_0000_0000`). The Pi 4 inbound viewport maps PCIe
`[0x4_0000_0000, 0x6_0000_0000)` onto RAM `[0, 0x2_0000_0000)`, so every
RAM frame's device address (≈ `0x4_xxxx_xxxx`) trivially exceeds the
CPU-physical top and the carve was refused before the controller was
touched. Fixed by bounding each side in its own address space:
`FrameDmaHost` now checks the frame's CPU-physical span against the CPU
window top and translates afterwards, and `bring_up_keyboard` passes
`open_discovered` the **device-visible** top (`inbound_pcie_base +
inbound_size` = `0x6_0000_0000`) to match `DmaSlab::phys()`. The redundant
`PcieBringup.dma_aperture_top` field (derivable from the windows) was
removed (§2.2).

That uncovered a fourth defect at the same `4101` xHCI stage, now
`err=length_out_of_range`, right after the two-function `4104` scan: the
register BAR could not be **mapped**. The VL805's BAR base read from PCI
configuration space is a *PCIe-bus* address — firmware assigns it inside
the outbound window (≈ `0xc000_0000`) — but `IdentityMmioMapper` only
permitted *CPU-physical* addresses, comparing the bus address against the
outbound **CPU** base (`0x6_0000_0000`). The bus address (≈ 3.2 GiB) is
nowhere near the CPU base (24 GiB), so `map_window` returned
`InvalidRegion` (surfaced as `LengthOutOfRange`) before the controller was
touched. Fixed by making the mapper bridge-aware: it now applies the
bridge's outbound `ranges` translation
(`outbound_cpu_base + (bus − outbound_pcie_base)`) to reach the
identity-mapped CPU address — exactly the bus→CPU resolution a host
bridge performs. The controller register block stays CPU-physical/identity
and is resolved first; because the Pi 4 regs island numerically falls
inside the outbound PCIe window, a request that only partially overlaps the
regs block is refused fail-closed rather than mis-translated (§5.4).
Host-proven by `keyboard_service::mapper_translates_a_bar_through_the_outbound_viewport`.

The `4105` map-decision event then localised a **fifth** defect at the same
`4101` stage. Its refused line carried `phys_base_hex=0` (`resolved_cpu_hex`
the `ffff…` sentinel): the VL805's BAR0 **address bits read zero** — the BAR
is sized and typed (a 64-bit memory BAR) but carries no base. Firmware
normally assigns a function's BARs, but resetting and re-enumerating the
root complex leaves the downstream function unassigned, so mapping it
targets physical address 0 and is refused. Assigning resources from the
host bridge's outbound window is the PCI core's job (the standard PCI
resource assignment), and nothing was doing it. Fixed by adding
[`PciBus::assign_bar`]: the USB bring-up now calls it before mapping the
BAR — it probes the BAR's size and type, and if the address bits read zero
places the BAR at the lowest size-aligned PCIe-bus address inside the
discovered outbound window (≈ `0xc000_0000`), writing both dwords for a
64-bit BAR and preserving the memory-type control bits. An already-based
BAR (firmware-assigned, or QEMU's) is left untouched, so the change is a
no-op everywhere a BAR is already programmed. The bridge-aware
`IdentityMmioMapper` then translates the assigned bus address to its
identity-mapped CPU base as before. Host-proven by
`assign_bar_places_an_unassigned_64bit_bar_in_the_window` (pci) and
`open_discovered_enables_mastering_and_reaches_the_controller` (usb, which
asserts the outbound window reaches `assign_bar`).

With BAR assignment in place the `4105` map-decision now succeeds for the
BAR (a second, `Info` line whose `resolved_cpu_hex` is the real outbound
CPU base, e.g. `0x6_0000_0000`, not the sentinel) — so discovery, DMA
carve, BAR assignment and BAR map all pass — yet the bring-up still
reported `4101 … err=out_of_range`, now *after* the BAR maps. That
`out_of_range` is a `RegisterWindow` bounds/alignment refusal
(`WindowError` → `OutOfRange`) or a DMA-address rejection
(`Layout::new`/`DmaProgram::is_plausible`) inside the controller bring-up
itself, so it could not be localised from the staged `4101`/`4105` lines
alone.

To measure it (rather than guess, §15.7), the USB bring-up was split:
`rustos_drv_bus_usb::wiring::open_discovered` now composes a public
`map_controller` (discovery + DMA carve + BAR assign/map, returning the
mapped `MappedXhci { window, dma }`) with `Xhci::open` + `UsbDevice::start`,
and `bring_up_keyboard` drives those three steps with **distinct** failure
messages (mapping / open / start) and a one-shot `4106` geometry line in
between (the carve's device-visible base/length against the aperture top,
the mapped BAR window length, and the controller's
`CAPLENGTH`/`DBOFF`/`RTSOFF`/`MaxSlots`/`MaxPorts`).

The next metal capture narrowed it further: the `4106` *carve* line
printed (`dma_phys=0x4_3b08_0000`, `dma_len=0x4000`, below the
`dma_aperture_top=0x6_0000_0000`, `bar_window_len=0x1000`) and then the
**open** stage failed `4101 … err=out_of_range` — but the `4106`
*geometry* second line never printed. That second line is logged only
*after* `Xhci::open` returns, so the refusal is inside `open` itself,
before it reports any geometry. The only `out_of_range` source inside
`open` is the register window's own bounds/alignment check: every
capability offset it reads is tiny and 4-aligned, so the refusal must be
the operational base `op_base = CAPLENGTH` being misaligned (a `CAPLENGTH`
that is not a multiple of four), or the BAR not decoding at all.

To measure *that* (rather than guess, §15.7) a one-shot `4107` raw probe
reads the first capability dwords straight off the mapped BAR —
`CAPLENGTH`/`HCIVERSION` (`0x00`), `HCSPARAMS1` (`0x04`), `HCCPARAMS1`
(`0x10`), `DBOFF` (`0x14`), `RTSOFF` (`0x18`) — *before* `Xhci::open`
interprets them, failing closed to the all-ones sentinel on a refused
read.

The metal `4107` capture showed every dword reading a **uniform**
`dead_dead`: not the all-ones master-abort pattern, and not real values,
but a constant poison across all offsets. That is the BAR *mapping*
correctly (the CPU access reaches the BCM2711 root complex) while the
VL805 itself does not decode. On a Raspberry Pi 4 the VL805's firmware is
loaded by the **bootloader EEPROM** (via the `VideoCore`), and on such a
board `VideoCore` (re)loads it on the **`PERST#` deassert edge** — the
same edge the bring-up drives, after which no
runtime VL805 reload. The root-complex bring-up therefore drives a proper
`PERST#` cycle (assert in `reset_controller`, deassert in `train_link`) so
that edge fires (see "Drive the `PERST#` cycle" below).

`open_controller` waits for the firmware-version register (`0x50`) to read
non-zero (`4118`) as a best-effort, diagnostic signal, then — **regardless of
its outcome** — records the firmware-version gate (`4123`) and waits for the
controller's capability block to come live (`4109`). The BAR poll checks for a
*live* value (a sane `CAPLENGTH` ≥ `0x20` and a plausible `HCIVERSION`, not the
`dead_dead`/UR/zero patterns) up to a bounded ~256 ms budget before
`Xhci::open` interprets the registers, turning "the controller just needed
time" into a clean bring-up while a controller that never decodes still fails
closed at `open` (§2.1 bounded / §2.9 fail closed). The config-space `0x50`
register is a VL805 vendor convenience, **not** the controller's readiness
signal — the capability block on the BAR is — so a `0` there does not abort
the bring-up before that block is probed.

The keyboard service wires a runtime `FirmwareReset`
(`keyboard_service::VideoCoreFirmwareReset`): when config `0x50` reads `0`,
`open_controller` issues exactly one `NOTIFY_XHCI_RESET` over
`rustos_vcmailbox` as a **best-effort** fallback (`4108`/`4121`/`4122`). Its
outcome no longer gates the bring-up. A metal capture that keeps `0x50 == 0`,
drops the reload, *and* keeps BAR reads at `dead_dead` (`4109 ready_hex=0`)
after the no-touch sequence is a boot-firmware handoff issue and fails closed
at `Xhci::open`; whereas a live BAR capability block (`4109 ready_hex=1`) lets
the keyboard enumerate regardless of `0x50`.

### The bridge memory window — why the BAR still read `dead_dead`

A later metal capture showed the reorder + readiness poll were necessary
but not sufficient: `4108 … Reloaded` (the firmware honoured the tag) and
`4109 … ready_hex=0` with `caplength_hciversion_hex` *still* uniform
`dead_dead` after the full 256-poll budget — the BAR mapped, the firmware
reloaded, yet every register read returned the BCM2711's abort poison. The
cause was not the reload at all but a missing root-complex bring-up step:
a PCI-PCI bridge forwards a *memory* transaction downstream only when the
address falls inside its **Memory Base/Limit** window (type-1 config
offset `0x20`), and the BCM2711 ships that register empty (base `0`, limit
`0`). The root-complex bring-up programmed the bridge *bus-number*
register (so *configuration* reads reached the VL805 — hence the `4104`
scan saw `1106:3483`) and the controller's CPU→PCIe outbound *translation*
(`MEM_WIN0`), but never the bridge memory window, so the root port
master-aborted every CPU access to the VL805's BAR even though config
reads succeeded. A full PCI enumerator sets
this; the windowed `mech_brcm` accessor does not.

The fix is `BrcmPcieRc::program_bridge_mem_window`, run during bring-up
right after the bus-number programming: it sets the bridge Memory
Base/Limit to cover the discovered outbound PCIe range
(`[outbound_pcie_base, outbound_pcie_base + outbound_size)`), the same
range BARs are assigned within. The register encodes only address bits
`[31:20]` (1 MiB granularity) and the non-prefetchable window decodes
below 4 GiB, so a window reaching the 4 GiB line fails closed (`AGENTS.md`
§5.4); the BCM2711's outbound window sits at `0xc000_0000`, well below it.

The bridge-window programming is host-tested
(`bring_up_opens_the_bridge_memory_window_so_bar_reads_are_forwarded`
asserts the window covers `0xc000_0000..0x1_0000_0000`); the live BAR read
is the remaining on-metal acceptance item. A healthy post-fix capture
prints `4108 … Reloaded`, then `4109 … ready_hex=1` once the controller
decodes, and the `4107` dwords that follow read a real `CAPLENGTH`.

### Enable the bridge Command register *after* the link is up

A PCI-PCI bridge forwards a downstream *memory* transaction only when
Memory Space Enable is set in its own Command register (config offset
`0x04`); a full enumerator does this once the link is up. Earlier
bring-up issued `BrcmPcieRc::program_bridge_command` during the
configuration phase — **before** `train_link` deasserts `PERST#` — and the
`4110` read-back caught it not sticking:

```
bridge_bus_numbers_hex=0000000000010100      (primary 0, secondary/subordinate 1 — OK)
bridge_mem_base_limit_hex=00000000fff0c000    (window 0xc000_0000..0xffff_ffff — OK)
bridge_command_status_hex=0000000010100000    (bridge command = 0x0000 — did not stick)
vl805_command_status_hex=0000000010100146     (VL805 command = 0x0146 — mem-space + bus-master set)
vl805_bar0_hex=00000000c0000004               (64-bit BAR0 at 0xc000_0000 — OK)
vl805_bar1_hex=0000000000000000               (BAR0 high dword — OK, window below 4 GiB)
```

The bridge Command register read back `0x0000` while the adjacent
bus-number (`0x18`) and Memory Base/Limit (`0x20`) writes — the same
direct bus-0 path — stuck. The register offset is therefore right; what
differs is *timing*: the integrated RC latches Memory Space Enable only
against a **live link**, so a write issued while `PERST#` is still
asserted is lost. A working boot chain on this exact board enables the
root port only once the link trains — i.e. *after* link-up.

The fix mirrors that: `BrcmPcieRc::bring_up` now calls
`program_bridge_command` **last**, after `train_link` and the fail-closed
`link_up()` confirmation, so Memory Space + Bus Master latch against the
trained link. Host-tested by
`bring_up_enables_memory_space_and_bus_master_on_the_bridge`, which also
asserts the command write follows the final `PERST#`-deassert write to
`RGR1_SW_INIT_1`.

This is the unifying explanation for the otherwise-puzzling triple
symptom. `VideoCore` reaches the VL805 over the **same configured PCI bus**
to load its firmware (`VideoCore`
expects a configured PCI bus), and that path runs through the
root-port bridge's memory window. With Memory Space Enable not latched,
*both* our CPU reads of the BAR *and* `VideoCore`'s `NOTIFY_XHCI_RESET`
firmware-load writes to the BAR master-abort — so the BAR reads
`dead_dead`, the config `0x50` firmware version stays `0`, and the mailbox
reload returns `response=0` with no effect, exactly the captured state.
The live BAR read (and the firmware version going non-zero) is the
remaining on-metal acceptance item — QEMU models no Pi PCIe/USB.

### The outbound-window inverted-mask root cause — measuring the memory path

A now-removed one-shot outbound-window read-back diagnostic (a former
`EventId(4111)` produced by an `outbound_window_readback` method, both since
deleted — `AGENTS.md` §2.14/§2.16) **pinned the root cause** during bring-up:
it read the outbound (CPU→PCIe) translation registers back off the trained
register block and carried `mem_win0_lo_hex=…c0000000` (PCIe base
`0xc000_0000`) and `pcie_status_hex=…b0` (data-link-active + phy-link-up)
right, but `mem_win0_base_limit_hex=0x00003ff0` **wrong** — under the
BCM2711's field order that decodes to a CPU base of `0x6_3ff00000` sitting
*above* the limit `0x6_00000000`, an inverted and empty window (see the
root-cause section below). With the outbound translation window decoding
nothing, every CPU access to the BAR master-aborted to `dead_dead`
regardless of firmware. That the same `dead_dead` reproduced on a
**known-good board whose USB works under other operating systems** confirmed
the fault was *not* the board and *not* the firmware — it was systematic in
this kernel, in a register we program. The `BASE`/`LIMIT` field-mask fix is
permanent and guarded by the driver regression test
`outbound_window_decodes_a_non_empty_range_covering_the_cpu_window`, which
reads the programmed window straight out of the mock register block; the
read-back log event was only a metal-diagnostic and was removed once
bring-up was confirmed.

### Drive the `PERST#` cycle; reload firmware only as a fallback

`0xdead_dead` is the **BCM2711 root complex's master-abort poison** (it is
distinct from RustOS's own all-ones `0xffff_ffff` diagnostic sentinel):
the RC returns it when a CPU memory access reaches the RC but no downstream
target decodes the PCIe address. On this Pi 4 the VL805's xHCI firmware is
loaded by the **bootloader EEPROM** (via `VideoCore`): the keyboard works
in the pre-boot firmware menu, and no
runtime VL805 reload is needed. On such a bootloader-EEPROM board `VideoCore` (re)loads the VL805
blob on the **`PERST#` deassert edge** — the only edge the
bring-up drives. The decisive metal datum was the VL805 **XHCI MCU
firmware version** (config `0x50`, the vendor firmware-version
register): it read `0`, and two sequences failed
identically — "assert `PERST#` **and** issue `NOTIFY_XHCI_RESET`
unconditionally" (the redundant second load that raspberrypi/firmware #1380
reports can kill the VL805) and "never assert `PERST#`, reload as a
fallback" (which denied `VideoCore` the deassert-edge trigger entirely).

The bring-up uses the gentlest no-touch-probe sequence — produce a single
`PERST#`-deassert edge from the handoff's already-asserted reset (no fresh
fundamental reset) and do not unconditionally reload:

- `BrcmPcieRc::reset_controller` **releases** the always-accessible RGR1
  bridge `sw_init` reset (`0x9210`) the previous boot stage left asserted,
  so the controller core and its MISC block come online (the first MISC
  access does not master-abort — see the bring-up-pause section above); it
  does **not** re-assert `sw_init`/`PERST#` or toggle the SerDes, either of
  which could drop the resident VL805 firmware. The link is retrained in
  `train_link`, which **deasserts the already-asserted `PERST#`**,
  producing the single deassert edge that re-triggers the `VideoCore` VL805
  firmware (re)load. Host-tested by
  `reset_releases_sw_init_without_re_asserting_a_fundamental_reset`,
  `bring_up_releases_sw_init_before_touching_misc_and_skips_the_serdes_toggle`,
  and `bring_up_trains_the_link_and_programs_the_windows`.
- `open_controller` waits for the VL805's XHCI MCU firmware version
  (config `0x50`, [`wait_for_firmware_loaded`], `4118`) to read non-zero
  **first** — the signal the vendor firmware-init sequence checks, read
  over the configuration path that works on metal while the BAR aborts. A
  non-zero version skips any reload, so RustOS never double-loads resident
  firmware. If the bounded wait stays `0`, the service issues exactly one
  `NOTIFY_XHCI_RESET` fallback (`4108`/`4113`) after the bridge is trained,
  the BAR is assigned, and command/memory decode is enabled, then waits for
  `0x50` again before the BAR capability-block wait (`4109`). Host-tested by
  `open_controller_reloads_firmware_when_version_stays_zero`,
  `firmware_reload_is_skipped_when_version_is_already_loaded`,
  `firmware_reset_failure_logs_the_mailbox_reason`, and
  `open_controller_stops_before_firmware_wait_when_mapping_fails`. A failed
  mailbox fallback logs a stable `reason` field (`timeout`,
  `firmware_error`, `malformed_response`, `bad_aperture`, `bad_geometry`, or
  `window`; `unknown` is reserved for a newer mailbox error reaching an older
  mapper) so the next metal capture identifies which firmware/mailbox
  invariant failed instead of collapsing every refusal to a bare `failed`.

`4110`/`4114` read the VL805's config + capability header (including the
`vl805_fw_version_hex` at config `0x50`) before and after the firmware wait /
optional fallback, so a metal capture shows whether firmware became resident
and whether the BAR decoded after the single safe reload attempt.

The latest captures show that the outbound-window fix moved the failure past
the BAR-decode stage: with the window now decoding correctly
(`mem_win0_base_limit` programmed to `0x3ff00000`), `4109` reports a live xHCI
capability header (`caplength_hciversion_hex=0x01000020`, `ready_hex=1`), and `4107`
reads plausible controller dwords (`HCSPARAMS1=0x05000420`,
`HCCPARAMS1=0x002841eb`, `DBOFF=0x100`, `RTSOFF=0x200`). The following
diagnostic then localised the remaining `4101 … err=device_fault` to
`stage=controller_ready_before_reset`, `USBCMD=0`, `USBSTS=0x805` — a halted
controller with `CNR` plus a latched Host System Error before RustOS has reset
it. That state is now handled by issuing the normal host-controller reset from
the halted controller and enforcing `CNR` only after the reset self-clears;
post-reset `CNR`, a stuck `HCRST`, or an unhaltable running controller still
fails closed with the same diagnostic fields.

The next capture moved the refusal to `stage=reset_self_clear`, `USBCMD=0x2`,
`USBSTS=0x815`: the reset command was accepted but did not self-clear while
the stale write-1-to-clear `HSE|PCD` latches were still set. `Xhci::open` now
clears only those latched `USBSTS` bits before asserting `HCRST`; `CNR` remains
fail-closed after reset, so the change removes the firmware-handoff latch
without trusting a controller that is still not ready after reset.

### Root cause — the outbound window's base/limit fields were defined swapped

The diagnostics showed the VL805 fully present (`1106:3483`, BAR0 based at
`0xc000_0000`, memory-space + bus-master enabled) while the capability block
stayed `dead_dead`. `4114` re-reading the function as correctly programmed,
plus the keyboard working under other operating systems on the same board/card/
firmware, redirected the search to the **outbound (CPU→PCIe) translation
window** — the one path config reads (which take the RC's internal `EXT_CFG`
route) do not exercise.

The defect was in `rustos_drv_bus_pcie_brcm::regs`: the BCM2711
**proprietary** `MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT` register (`0x4070`)
packs the window **limit** in bits `[31:20]` and the **base** in bits
`[15:4]`. But
`MEM_WIN0_BASE_LIMIT_BASE_MASK`/`..._LIMIT_MASK` were defined with the two
halves transposed (`0xfff0_0000` / `0xfff0`), so
`program_outbound_window` wrote the base into the limit's half and
vice-versa. For the Pi 4 window (CPU `0x6_0000_0000`, 1 GiB) that produced
the metal-captured value `0x00003ff0`, decoding to base `0x6_3ff00000`
**above** limit `0x6_00000000`: an inverted, empty window that decoded no
address, so every CPU access to the VL805's BAR master-aborted to
`dead_dead` regardless of the firmware state. The field positions are now correct; the expected Pi read-back is
`mem_win0_base_limit_hex=0x3ff00000` with `base_hi=limit_hi=0x6`.

The fix swaps the two mask constants so the base lands in `[31:20]` and the
limit in `[15:4]`, programming a window covering CPU
`0x6_0000_0000..0x6_3fff_ffff` that maps to PCIe `0xc000_0000` (the VL805's
BAR). Host-proven by
`outbound_window_decodes_a_non_empty_range_covering_the_cpu_window`, which
decodes the full CPU base/limit with the *hardware* field positions
(independent of the named constants, so a re-swap is still caught) and
asserts the window is non-empty and covers the outbound CPU range — it fails
before the fix (`base 0x6_3ff00000 > limit 0x6_00000000`) and passes after.
The live keyboard coming up is the remaining on-metal acceptance item (QEMU
models no Pi PCIe/USB, `AGENTS.md` §0.4).

### The D5d user-space move regressed two things — DMA coherency and the diagnostics

Moving the keyboard bring-up out of the in-kernel scaffold into the
autoloaded user-space `drivers/input/usb_kbd` process (the §4 steady state)
silently dropped two things the metal-debugged scaffold relied on:

1. **DMA coherency.** The BCM2711 PCIe root complex is **not** I/O-coherent
   (it does not snoop the CPU caches). The in-kernel scaffold's
   `keyboard_service::FrameDmaHost` therefore wired aarch64
   `clean_invalidate_dcache_range` into every device-shared `DmaSlab`, but
   the user-space `RtDriverHost` carved its DMA with `coherency = None` — and
   EL0 cannot do cache maintenance (`SCTLR_EL1.UCI` is clear), so it could
   not have anyway. The carve was plain cacheable RAM, so the first
   command-ring TRB the driver wrote sat in a dirty cache line the controller
   never saw: `UsbDevice::bring_up_keyboard` stalled on the very first
   Enable Slot (no completion ⇒ the budget-poll timeout), so the onboard
   hub's downstream ports were never powered — the "no device power" metal
   symptom, the chain exiting ~644 ms after the last syscall with no `delay`
   ever reached. Fixed structurally and arch-neutrally: the kernel DMA carve
   (`kernel/mem::dma`) maps the device-shared buffer with the new
   `PageFlags::DMA_COHERENT` (the W5b-4 HAL attribute), which on aarch64 is
   **Normal Non-Cacheable** (`MAIR` index 2). The buffer is coherent by
   construction with no per-access maintenance and no EL0 privilege, the
   driver stays platform-neutral (`AGENTS.md` §2.20), and Normal-NC — unlike
   Device-nGnRE — still permits the ordinary ring/context accesses the engine
   makes. x86_64/riscv64 are coherent and map it cacheable.
2. **The per-stage diagnostics.** The scaffold logged `4101`/`4106`/`4126`
   per-stage records; the user-space driver exited silently with code `82`.
   Restored as a user-space one-shot structured `log_emit` record:
   `rustos_hid::bring_up_boot_keyboard_diagnostic` returns a
   `KeyboardBringupError` (the failing `BringupPhase` plus the `Xhci::open`
   reset sub-stage + `USBCMD`/`USBSTS`, or the enumeration
   `stage`/`completion`/`reject`/`evtype` breadcrumbs + root-port `PORTSC`),
   which `usb_kbd` emits as event `4126` (and a `4101` "controller up"
   beacon) through `rustos_rt::LogSink`, gated on `CAP_LOG_EMIT`
   (`AGENTS.md` §15.7 / §19.4). A non-I/O-coherent stall now reads
   `phase=enumerate stage_hex=2 completion_hex=0` — the historical
   DMA-not-visible signature — instead of a blind exit.

Both are host-proven (the coherent-DMA leaf in `kernel/arch/aarch64`'s
`paging_tests`, the diagnostic surface in `lib/hid`'s `service_tests`); the
live keyboard coming up over the user-space chain is the on-metal acceptance
item (QEMU models no Pi PCIe/USB, `AGENTS.md` §0.4).

### DMA ordering — the barrier the non-coherent PCIe master needs

Mapping the device-shared buffer Normal **Non-Cacheable** (above) makes it
*coherent* — no cache maintenance — but **not** *ordered* with respect to the
controller. On AArch64, Normal-NC stores and the Device-memory doorbell store
are not mutually ordered for this PE without an explicit barrier, and the
controller writes an event-ring entry's body before its cycle bit. So the
user-space driver must, like every OS driver on a non-I/O-coherent master,
issue a barrier: a store barrier after publishing TRBs and before the
doorbell, and a load barrier after observing a fresh cycle bit and before
reading the entry body. With no barrier, the controller could observe a
doorbell before the TRBs it announces (a stall), and the driver could read a
new cycle bit paired with the *previous* entry's stale TRB pointer — the metal
`id=4126 phase=enumerate stage_hex=7 completion_hex=1 reject_hex=2` capture
(a SUCCESS Transfer Event whose pointer mismatched the awaited status TRB,
`REJECT_ADDRESS_MISMATCH`). The gap was latent until the EMMC2 speed-up made
the CPU side outrun the controller's write-back window.

`core::sync::atomic::fence` is **not** the fix: on AArch64 it lowers to an
*inner*-shareable `dmb ish`, which does not order accesses against the
outer/system-domain PCIe DMA master. The barriers live in the new
`rustos-dma-barrier` crate (the user-space analogue of `rustos-abi-trap`'s
§1 asm carve-out): `dma_wmb()` = `dmb oshst`, `dma_rmb()` = `dmb oshld`. The
arch-neutral `rustos-usb` engine calls them at the controller-start and
doorbell handoffs and in `poll_event` (cycle bit first, `dma_rmb`, then the
entry body). x86_64 (`sfence`/`lfence`) and riscv64 (`fence iorw,iorw`) get
the equivalent; host/wasm32 are a no-op. The live keyboard is the on-metal
acceptance item.

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
- The secondary-stack pool is **not** a fixed `.bss` reserve (which would
  cap the machine at a compile-time core count, `AGENTS.md` §24.1). The
  caller registers a `smp::SecondaryStackPool<N>` sized to its machine's
  discovered core count (a `static` for the allocator-free bins); its
  `register` publishes the pool base and per-core stride to the `smp.s`
  trampoline (ordered ahead of any `CPU_ON` by a `dsb sy`) and the covered
  count to `smp::is_valid_cpu`. Registration is set-once, and every id is
  invalid until a pool is registered, so an unstarted system fails closed.
- `smp::start_secondary` validates the dense `CpuId` against the registered
  pool's count, confirms an entry is installed, then issues a PSCI `CPU_ON`
  (`kernel/arch/aarch64::psci::cpu_on`) through the conduit (`hvc`/`smc`)
  the `fdt` reader discovers, entering the core at the `smp.s` trampoline.
  The trampoline masks interrupts, computes the core's stack top as
  `base + (cpuid + 1) * stride` from the published pool globals (indexed by
  the dense id PSCI passes as the `context_id`), and tail-calls the
  installed entry. It fails closed (`StartCpuError`) on an out-of-range id,
  a missing entry, or a PSCI error rather than assuming the core came up.
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

### Splitting a block: the guard-page fault-form (P6 follow-on, G1)

The boot path identity-maps RAM with coarse 1 GiB / 2 MiB *block*
descriptors (`new_identity_gigapages`), and a block has no per-4 KiB leaf
to clear, so an individual page inside it cannot be unmapped. The kthread
kernel-stack guard page's *deployment* form — turning a stack overflow
into an immediate hardware fault by unmapping the guard page rather than
detecting a poison canary at the next reschedule (`AGENTS.md` §2.17,
`plans/PI.md`) — needs that region re-expressed at 4 KiB granularity
first. `AddressSpace::split_block(vaddr)` does it: the 1 GiB L1 block
covering `vaddr` becomes a table of 512 × 2 MiB blocks, then the covering
2 MiB block becomes a table of 512 × 4 KiB pages. The shared
`shatter_block_into` helper reproduces the block at the finer granularity
by preserving every attribute bit (`desc & !ADDR_MASK`) and only
recomputing each sub-entry's output address, setting `TABLE_OR_PAGE` for
the L3 page leaves — so the same memory keeps mapping with identical
permissions (one attribute vocabulary, §2.2).

The split is **break-before-make-free for the running region**: it only
ever *adds* table levels that reproduce the existing translation, never
invalidating a live address, so it is safe to run against the active
regime — unlike a naive block→table swap, whose break window would
momentarily unmap the kernel's own running stack/code (the reason the
fault-form is staged G1..G3 in `plans/PI.md` rather than shattering the
block the CPU executes in). It is idempotent (a level already a table is
left untouched) and fails closed (`Misaligned` / `NotMapped` /
`PoolExhausted`). After a split, the single 4 KiB page unmaps through the
existing `MmuAddressSpace::unmap` and its stale TLB entry is dropped with
`TlbShootdown::flush_page`.

The table arithmetic is host-tested (`paging_tests.rs`:
`split_block_shatters_a_gigapage_to_pages_preserving_the_identity_mapping`,
`split_block_then_unmap_tears_down_exactly_one_page`,
`split_block_preserves_device_attributes`,
`split_block_is_idempotent_and_fails_closed`), and the live mechanism is
proven on `-M virt` by `tests/integration/stack_guard_qemu_aarch64`: build
an identity space, `split_block` a RAM block, enable the MMU,
write+read-back a sentinel through the guard page (the split preserved the
mapping live), then `unmap` + `flush_page` that one page and read it — the
MMU raises a synchronous data abort the fault handler reports as PASS.

### A guard-page arena: the boot mapping (P6 follow-on, G2)

G1 supplies the per-page primitive; G2 lays out *where* the guarded
kthread kernel stacks live so the eventual guard-page unmap (G3) never has
to break-before-make the coarse block the CPU is currently running on or
stacked in. The boot path reserves a **guard arena** — a 2 MiB-aligned,
2 MiB region carved out of the discovered usable RAM window, above the
kernel image (`rustos-kernel::mem_map`) — and marks it
`RegionKind::Reserved` so the frame allocator never hands its frames to
another use (`AGENTS.md` §4: a guard page on shared frames would corrupt
an unrelated allocation). `build_memory_map` now returns a
`MemoryLayout { map, arena }` whose regions tile the window exactly
(reserved kernel image, optional usable head, the reserved arena, usable
remainder); a window too small for a 2 MiB block degrades to no arena
(fail closed), leaving the guard in its software-canary form.

`AddressSpace::prepare_guard_arena(base, len)` re-expresses the arena at
4 KiB granularity by applying `split_block` to every 2 MiB block the arena
spans. Because the split only *adds* table levels (it is the same
break-before-make-free operation above), preparing the arena over the
*active* boot tables changes no translation and needs no TLB maintenance;
it is idempotent and fails closed (`Misaligned` / `NotMapped` /
`PoolExhausted`). `boot_aarch64` keeps the live boot `AddressSpace`
(`enable_mmu_and_vectors` returns it) and prepares the arena after the RAM
window is discovered, recording a `guard_arena_prepared` audit field. The
crucial property is that the arena is its own 2 MiB block, **distinct from
the block holding the running code and stack** — so unmapping a guard page
in it later (G3) never touches the running region.

The carving and tiling arithmetic is host-tested (`mem_map.rs`), the
range-split is host-tested (`paging_tests.rs`:
`prepare_guard_arena_splits_every_covering_block_preserving_translation`,
`prepare_guard_arena_is_idempotent`, `prepare_guard_arena_fails_closed`),
and the live mechanism is proven on `-M virt` by
`tests/integration/stack_arena_qemu_aarch64`: prepare a 2 MiB-aligned arena
that is its own block, enable the MMU, write+read-back a sentinel through a
guard page, `unmap` + `flush_page` it, prove the running stack (a
*different* 2 MiB block) and a neighbouring arena page still work, then
read the unmapped page — the MMU raises a synchronous data abort the fault
handler reports as PASS.

### Promoting the split onto the Arch HAL (G3a)

The block-split primitive is now part of the architecture-neutral Arch HAL
`AddressSpace` surface (`rustos_arch_api::mmu`, `AGENTS.md` §17.2), so the
kernel reaches it through one vocabulary rather than naming a concrete
port. Two members were added:

- `AddressSpace::block_split_support() -> BlockSplit` — each port's honest
  declaration, modelled on the §19.1 / §19.10 `Mitigation` / `Tagging`
  profiles: `Supported`, justified `Unsupported`, or tracked `Pending`.
  aarch64 reports `Supported`; riscv64 and x86_64 report `Pending` (their
  Sv39 / four-level huge-page splits land with each port's own guard-page
  fault-form), with a non-empty tracking note the `mmu::conformance`
  vertical enforces.
- `AddressSpace::split_block(vaddr)` — the HAL view of the operation. Its
  default fails closed with `MapError::Unsupported` so a non-supporting
  port never silently no-ops (`AGENTS.md` §2.9). The aarch64 impl forwards
  to its inherent, fully-tested `split_block` body, so there is one
  implementation reached either directly (the boot path and the G1/G2
  verticals) or through the HAL trait (`AGENTS.md` §2.2).

The `mmu::conformance` suite gained a block-split honesty check (every
port: the declaration is justified, and a non-`Supported` port fails
`split_block` closed); aarch64's `paging_tests` additionally proves the
HAL `split_block` reaches the inherent body over a `dyn AddressSpace`.

### Promoting the arena onto the Arch HAL (G3b)

`AddressSpace::prepare_guard_arena(base, len)` — the arena form of the
split, applied to every coarse block an arena spans (G2) — is likewise now
a member of the architecture-neutral HAL `AddressSpace` surface. Its
default fails closed with `MapError::Unsupported`, so a port whose
`block_split_support` is not `Supported` falls back to the software canary
guard rather than silently pretending the arena was hardened (`AGENTS.md`
§2.9 / §2.17). The aarch64 impl forwards to its inherent, fully-tested
`prepare_guard_arena` body — one implementation reached either directly
(the boot path and the G2 vertical) or through the HAL trait (`AGENTS.md`
§2.2). The `mmu::conformance` honesty check now also requires a
non-`Supported` port to fail `prepare_guard_arena` closed (riscv64 and
x86_64 do, matching their `Pending` split); aarch64's `paging_tests`
proves the HAL `prepare_guard_arena` reaches the inherent body over a
`dyn AddressSpace`.

### Routing the kthread stack through the arena (G3b-2)

Both spawn paths now draw their kthread kernel stack from the reserved
guard arena with its guard page genuinely **unmapped in the task's own
page-table root**, so an overrun of a task's kernel stack takes a
synchronous data abort under that task's `TTBR0_EL1` rather than a
next-reschedule poison-canary detection. The pieces:

- A grow-and-shrink block allocator, `stack_arena::KTHREAD_STACK_ARENA`
  (`rustos-kernel`), hands kthread kernel stacks out of the boot-reserved
  arena (`mem_map`, G2). `boot_aarch64` `install`s it from the carved
  arena `(base, len)`; each `alloc` returns an `ArenaStack` — a one-page
  guard region below the usable `KTHREAD_STACK_BYTES` stack, identical in
  geometry to the heap-backed `BoxStack`. When the boot block is full it
  chains a fresh 2 MiB block from the live `FrameAllocator`
  (`FrameArenaGrow`); when a task exits its `ArenaStack` is dropped and the
  region returns to its owning block (`StackArena::free`), and an idle
  chained block is zeroed and returned to the allocator (`FrameArenaShrink`)
  under a one-free-block grace, so the capacity rises *and* falls without
  thrashing (`AGENTS.md` §24.1). The boot-carved block is never returned.
- **PID 1 `init` (G3b-2-i):** `init_spawn` allocates one region, then — on
  `init`'s *own* concrete `arch` address space, **before** it is switched
  to — calls `split_block(guard)` (re-expressing the coarse identity block
  at 4 KiB) followed by `unmap(guard)`. Doing it before activation
  disturbs no live access and needs no TLB maintenance. The boxed stack is
  handed to `kernel/core` through the `InitSpawnCtx::admit_init` `stack`
  parameter (a `Box<dyn KernelStack + Send>`, so the concrete stack source
  never leaks into the object-safe boundary, §17.4) and admitted via
  `spawn_user_kthread_with_stack_live` — `init` also passes a retained
  `LiveSpace` through the seam's `live` parameter so its `mem_map` /
  `mmio_map` mutate its own address space (see *Architecture → Memory* §7e,
  `plans/PI.md` 5d-0-ii (b′)-2).
- **The runtime `spawn` syscall (G3b-2-ii):** `spawn_producer` does the
  same, on the child's *own* `arch` root — which it builds but **never
  switches to** (the spawning caller keeps its own `TTBR0_EL1`), so
  `split_block`/`unmap` only touch the child's tables through the caller's
  identity window, disturb no live access, and need no TLB maintenance.
  `kernel/core` grew the matching `SpawnCtx::admit_process` `stack`
  parameter (mirroring `admit_init`), routing the child through
  `spawn_user_kthread_with_stack_live` with a retained `LiveSpace` (the
  `live` parameter). So the session and anything it launches now run on an
  arena-backed, hardware-guarded kernel stack too, and map their own
  `mem_map` / `mmio_map` regions into their own retained space.
- If no arena region is available, or the split/unmap could not be
  applied, either seam falls back to a software-canary `BoxStack` — neither
  ever runs on an unguarded stack (fail closed, `AGENTS.md` §2.9 / §2.17).

`ArenaStack::check_guard` keeps the default `Ok(())`: the guard page is
unmapped, so the hardware fault is the defence (there is no poison canary
to scan, and reading the page under the dispatcher's root would
false-positive). The allocator's bump arithmetic is host-tested
(`stack_arena` unit tests), and the existing aarch64 `spawn_init` /
`spawn_session` / `wait` QEMU verticals prove `init` still reaches EL0,
writes its banner, and supervises the session — `init` and the session
both on arena-backed stacks.

### Proving the overrun fault-form (G3c)

G3b-2 unmaps each kthread stack's guard page; **G3c** proves the payoff on
the `virt` board: a live, scheduled kthread that overruns its kernel stack
takes a **synchronous data abort the instant the overrun crosses the guard
page**, rather than the next-reschedule poison-canary detection the
heap-backed `BoxStack` falls back to.
`tests/integration/stack_overrun_qemu_aarch64`:

- builds a stage-1 identity `AddressSpace`, prepares a 2 MiB-aligned guard
  arena (`prepare_guard_arena`, G2), and carves one kthread stack region
  `[guard page | usable stack]` out of it;
- installs the EL1 vectors + a `fault` handler, enables the MMU, then
  `unmap`s the guard page through the Arch HAL + `flush_page`s it — exactly
  the G3b-2 production mechanism;
- builds the live `rustos-kernel-sched-eevdf` `Scheduler` over `Aarch64Arch`
  and admits a kthread on that stack through
  `kernel_core::spawn_kthread_with_stack` (the production runtime path), then
  drives the cooperative `step` loop;
- the kthread body overruns its stack by touching the highest byte of the
  guard region — the first byte a contiguous downward overrun crosses.
  Because that page is unmapped the access faults synchronously *while the
  kthread runs*; the abort is taken on the still-healthy usable stack above
  the guard, so the EL1 trampoline does not nest-fault. The handler confirms
  the cause (`ESR_EL1` is an abort) and the faulting address (`FAR_EL1`
  inside the guard page) and reports PASS via the semihosting finisher.

A regression that left the guard page mapped lets the body return cleanly;
the drain loop then reports FAILURE explicitly rather than passing
(`AGENTS.md` §2.9). The vertical is enrolled in
`tools/xtask/src/commands/qemu_tests.rs` (single CPU, 60 s) and is **verified
green under QEMU on `-M virt`**. With G3c landed the guard-page fault-form
(G1–G3) is complete on aarch64.

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
classifies each core's rating against the machine's peak with the pure
`kernel/arch/aarch64::hetcore::class_for_capacity`: the highest rating
present is the performance tier, and any core rated strictly below it is
an efficiency core. Two device-tree passes (find the peak, then classify)
carry no fixed-size buffer, so the classification scales to the
caller-sized per-CPU table rather than a fixed compile-time CPU ceiling (`AGENTS.md`
§24.1). A homogeneous machine — every rating equal, no ratings at all, or
a malformed tree — leaves every core a performance core, the safe Arch HAL
default; a core with no advertised rating is never guessed down. The
classified table is read back through the `core_class` override, which
returns the performance default for an out-of-range `CpuId` (totality,
never a panic).

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
  `paging::AddressSpace::new_identity_gigapages` (under the default
  Device gigapage mask: GiB 0 Device memory for the GIC/PL011/virtio-MMIO
  apertures, RAM Normal-cacheable). The MMU-off
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
shared `fw_cfg` MMIO transport (`rustos-fwcfg`'s `MmioDma`) the
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
