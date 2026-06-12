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
| `dtoverlay` | `disable-bt` | detach Bluetooth from the PL011 so `UART0` is the primary UART on the GPIO 14/15 header |
| `init_uart_clock` | `48000000` | pin the PL011 reference clock to the 48 MHz the kernel's baud-divisor arithmetic assumes (`uart_init::UART_CLOCK_HZ`) |
| `init_uart_baud` | `9600` | the firmware's own early output (if any) matches the kernel's line setting — the kernel then programs the PL011 itself to **9600 baud, 8 data bits, no parity, 1 stop bit** (`uart_init`) |
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
host-tested `rustos_kernel::mem_map` module (the analogue of the riscv64
boot pipeline's `rustos_kernel::boot_riscv64::build_boot_memory_map`).
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
entry point. The `_start` trampoline (`boot.s`) follows the Linux aarch64
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
   9600 baud from the pinned 48 MHz reference clock (312 + 32/64),
   `LCR_H` 8N1 + FIFOs (which latches the divisors), `IMSC` all-masked
   (the console polls), then `CR` re-enable (`UARTEN|TXE|RXE`).

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
§10). The `video` module brings the screen console up on boards whose
display pipeline is owned by the `VideoCore` firmware (the Raspberry
Pi):

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
- **Rendering.** A fixed-grid text console draws the shared 5×7 glyph
  atlas (`rustos_font::glyphs` — one font definition, `AGENTS.md`
  §2.2) at an integer scale chosen from the display height
  (`height / 360`, clamped to 1…4: 480p → 1×, 1080p → 3×). The grid
  is a ring — the cursor wraps to a cleared top row instead of
  scroll-copying megabytes per log line (`AGENTS.md` §2.16). After the
  MMU and caches come on, each write cleans the touched scanlines to
  the point of coherency (`dc cvac` + `dsb`) so the firmware scan-out
  sees them; rendering is serialised by a private DAIF-masking
  spinlock (deliberately not `lib/sync` — feature unification across
  the single aarch64-none test-matrix build would compile its
  alloc-backed `epoch` module into the minimal, allocator-free QEMU
  binaries; the carve-out is documented at the lock).
- **Routing.** The **boot-log** path (`serial::ConsoleWriter`, the log
  sink) routes by build profile. A **release build** renders to the
  screen when `video::is_active` and falls back to the UART otherwise.
  A **debug build** (`cfg(debug_assertions)`) routes the whole
  log/debug stream to the **UART instead** — even while a login session
  owns the UART — so a serial capture of a development boot carries the
  full diagnostic stream while the screen stays clear for the
  user-facing session; with no UART discovered the bounded transmit
  drops the bytes and the screen is never the debug log's sink. The
  **stream** path is split per console (`plans/PI.md`
  P11): `boot_aarch64` installs `[VideoConsole, UartConsole]` through
  `BootInfo::with_consoles` when the framebuffer console is active
  (else `[UartConsole]`). `VideoConsole` writes through
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
  own stream backing with its **own login session** and never touches
  the display. The UART transmit wait is **bounded**
  (`console::tx_wait`): a transmitter that never drains — e.g. a
  flow-blocked PL011 still attached to the Bluetooth chip — is declared
  wedged after `TX_POLL_BUDGET` polls and bytes are dropped (one cheap
  poll each, recovering the moment the FIFO drains) rather than hanging
  the kernel on its first log line (`AGENTS.md` §2.1).

Fail closed (`AGENTS.md` §2.9): no mailbox node (QEMU `virt` — the
UART-backed verticals are unchanged), a detached display, or any
rejected/malformed firmware answer leaves the UART as the console. The
discovery, bring-up (over the protocol-faithful mock firmware — QEMU
does not model the `VideoCore`), geometry policy, and renderer are
host-unit-tested; rendering on a real HDMI display is an on-metal
acceptance item like the rest of the Pi peripherals.

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
  the same residue hazard Linux's `head.S` invalidates its idmap tables
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
    node's `dma-ranges` (`fdt::dma_ranges_aperture`: the 3-cell child
    PCI address stepped over, the 2-cell parent CPU base and size
    decoded). It is emitted as `HwResource::dma(top, len)` where `top`
    is the *exclusive* upper bound of the reachable CPU-physical window
    (`0xC000_0000` — the low 3 GiB of SDRAM — on the Pi 4) and `len`
    its extent; the VL805 wiring carves its xHCI DMA region below
    `top`.
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
  `spawn_user_kthread_with_stack`.
- **The runtime `spawn` syscall (G3b-2-ii):** `spawn_producer` does the
  same, on the child's *own* `arch` root — which it builds but **never
  switches to** (the spawning caller keeps its own `TTBR0_EL1`), so
  `split_block`/`unmap` only touch the child's tables through the caller's
  identity window, disturb no live access, and need no TLB maintenance.
  `kernel/core` grew the matching `SpawnCtx::admit_process` `stack`
  parameter (mirroring `admit_init`), routing the child through
  `spawn_user_kthread_with_stack`. So the session and anything it launches
  now run on an arena-backed, hardware-guarded kernel stack too.
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
