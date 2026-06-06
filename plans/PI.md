# PI.md — Booting RustOS on a real Raspberry Pi 4 (BCM2711, aarch64)

This is the staged build plan for taking the `kernel/arch/aarch64` port
from "boots on the QEMU `virt` board" to "boots a real Raspberry Pi 4
(BCM2711) into user mode and, ultimately, the desktop / window manager".

`AGENTS.md` is binding — read it, `PLAN.md`, and `plans/WIRING.md` first.
Every rule in this file is binding too. The continuation prompt for fresh
contexts is `.junie/next-pi-prompt.md`.

**Note:** `abi-v1` is *not* frozen, despite what `AGENTS.md` / `PLAN.md`
say — the standing task direction supersedes that language. Changing a
`lib/abi` type today is allowed; it requires regenerating the C header
(`cargo xtask c-header --write`), which the drift guard enforces.

---

## 0. Scope and binding decisions

1. **One board, the Raspberry Pi 4 (BCM2711).** This plan targets the
   Pi 4 / Pi 400 (BCM2711, Cortex-A72, GIC-400) specifically. The Pi 3
   (BCM2837, no GIC) and Pi 5 (BCM2712, RP1 southbridge) are explicitly
   out of scope here; they reuse this work as a later board port. Each
   board difference that surfaces is recorded, never silently assumed.

2. **HAL-first, never `virt`-vs-Pi `cfg` switches (§17.2 / §2.2).** The
   QEMU `virt` board and the Pi 4 are two *boards* of the **same**
   `aarch64` architecture. Their differences — UART model and base, the
   interrupt-controller base, the RAM/MMIO map, the boot protocol, the
   mailbox/framebuffer — are **runtime board data discovered from the
   device tree**, never `cfg(board = …)` forks of the port (that would be
   the §2.2 duplication / §2.3 bloat this charter forbids, and the
   §17.2 burn-down already moved discovery behind `PlatformDiscovery`).
   The single legitimate per-board artefact is the **boot stub + linker
   script + load address** (the `AGENTS.md` §1 "boot stubs" carve-out for
   architecture-required assembly), because that is fixed before any tree
   is parsed.

3. **Discovery is the contract.** Everything the kernel needs to talk to
   Pi 4 hardware — the `/memory` map, the UART (`compatible`,
   `reg`), the GIC-400 (`reg` for GICD/GICC), the timer PPIs, the mailbox,
   the SD host, the USB host — is read from the Pi 4 device tree through
   the shared `lib/fdt` reader and normalised into `rustos_abi::hwtree`
   by `kernel/arch/aarch64::platform::FdtDiscovery` (§18.2). The MMIO
   bases currently hard-coded as `virt` constants
   (`serial::PL011_BASE`, `gic::{GICD_BASE,GICC_BASE}`) must become
   discovered values threaded from the tree, not compile-time constants.

4. **No hard-coded device list standing in for detection (§18.5).** The
   Pi 4 peripherals autoload through the §18.3 `devmgr` match path against
   driver bind tables, exactly as on `virt`. "It's a Pi, just poke the
   known addresses" is forbidden.

5. **Fail closed, no hacks (§2.1 / §2.9 / §5.4).** No
   `unwrap`/`expect`/`panic!` in production paths, no `unsafe` without a
   `// SAFETY:` block plus a test, no retry-until-it-works bring-up, no
   "boots if you squint" milestones. A primitive the Pi genuinely lacks
   (or that is deferred) is declared honestly, mirroring §0.4 of
   `plans/WIRING.md`.

6. **Two proving grounds, both required.** Every stage that *can* be
   proven in emulation lands a `qemu-system-aarch64 -M raspi4b` (or
   `raspi3b` where the Pi-4 model is unavailable) vertical **in addition
   to** the existing `-M virt` verticals — so the board-discovery path is
   exercised in CI without hardware. Stages that can only be proven on
   metal (real mailbox/HDMI, real SD/USB timing) land a documented
   **hardware bring-up checklist** and a UART-log capture as the
   acceptance artefact, since CI has no Pi attached.

7. **Docs + tests are part of every stage (§7 / §13).** Each stage
   updates `docs/src/platform/aarch64.md` (and adds
   `docs/src/install/raspberry_pi.md` when Stage 8 image work lands),
   plus `PLAN.md` and this file, in the same change. Tests are never
   deferred.

8. **One increment per landing.** Land one complete, fully-gated stage,
   update `PLAN.md` + this file, refresh `.junie/next-pi-prompt.md`, then
   start the next.

---

## 1. Baseline — where the aarch64 port stands today

`kernel/arch/aarch64` is at QEMU-`virt` parity with x86_64 (`plans/WIRING.md`
W6/W7/W17): EL1 boot trampoline with an EL2→EL1 drop (`boot.s`), PL011
console (`serial`), GICv2 (`gic`), generic-timer preemption (`preempt`),
stage-1 MMU (`paging`), `svc` syscall entry, `eret` user entry
(`userentry`), context switch, PSCI `CPU_ON` SMP bring-up (`psci` + `smp`),
per-CPU storage (`percpu_hal`), side-channel + memory-tagging profiles,
and FDT → `hwtree` discovery (`platform` + `fdt`). All of it is proven
under `qemu-system-aarch64 -M virt`.

**What is `virt`-specific (the Pi-4 gap):**

| Concern | Today (`virt`) | Pi 4 (BCM2711) |
| --- | --- | --- |
| Production kernel binary | none — aarch64 boots only via per-test bins; `rustos-kernel/build.rs` wires **only** `x86_64` | a real `aarch64-unknown-none` `rustos-kernel` image is required |
| Boot protocol | QEMU `-kernel <elf>`, Linux hand-off `x0 = DTB` at EL2/EL1 | Pi firmware (`start4.elf`) loads `kernel8.img` at `0x80000`, enters EL2, `x0 = DTB` (Pi firmware-supplied) |
| Load address / linker | `0x4020_0000` (`aarch64-virt.ld`) | `0x8_0000` (needs an `aarch64-rpi4.ld`) |
| Console UART | PL011 @ `0x0900_0000` (fixed const) | PL011 @ `0xFE20_1000` *or* mini-UART (AUX) @ `0xFE21_5040`, base discovered |
| Interrupt controller | GICv2 @ GICD `0x0800_0000` / GICC `0x0801_0000` (fixed const) | GIC-400 @ GICD `0xFF84_1000` / GICC `0xFF84_2000`, base discovered |
| RAM base | `0x4000_0000` | `0x0` (low 1 GiB; up to 8 GiB with the `>3GiB` window) |
| Display | virtio-gpu / ramfb | VideoCore mailbox framebuffer → `drivers/display/rpi_hvs` (HVS) |
| Storage | virtio-blk-mmio | EMMC2 SD host controller (`drivers/storage`) |
| Input | virtio-keyboard-mmio | USB HID via the VL805/DWC2 USB host (`drivers/bus/usb`) |
| Image builder | none (`tools/mkimage` empty) | `images/rustos-aarch64-rpi.img` (FAT boot partition + firmware blobs) |

`drivers/display/rpi_hvs` already exists (HVS layer compositor, mock-host
tested) and consumes an `HvsConfig` the boot capability provides; it has
no hardware vertical yet.

---

## 2. Strategy

The work splits into three arcs:

- **Arc A — Boot to a UART prompt on the `raspi4b` model (P1–P3).**
  Pure-software, fully CI-provable under `qemu-system-aarch64 -M raspi4b`.
  This de-risks the boot protocol, the linker/load address, the console,
  and board discovery without any hardware.
- **Arc B — Reach user mode on the Pi (P4–P6).** Interrupt controller,
  timer, MMU, and the live scheduler over discovered Pi bases; spawn an
  init process. Still mostly `raspi4b`-provable, with a metal checklist.
- **Arc C — Real peripherals + bootable image + desktop (P7–P10).**
  Mailbox/framebuffer, SD, USB-HID, the SD-card image, and finally the
  WM/taskbar on the HVS path. These need real hardware to *fully* prove.

Land them in order; each stage's "Done when" gate is binding.

---

## 3. Stages

> Status legend: `[ ]` not started · `[~]` in progress · `[x]` done.
> Keep this list and the PLAN.md Stage-3b/Stage-8 entries in sync.

### P0 — Pi-4 facts of record (no code) `[x]`

Pin down the BCM2711 numbers this plan depends on, in
`docs/src/platform/aarch64.md` under a new "Raspberry Pi 4 (BCM2711)"
section, so later stages cite one authoritative source (§13, no guessing
per §15.7):

- Low-peripheral MMIO base `0xFE00_0000` (the `0x7E00_0000` VC bus alias
  mapped to ARM physical `0xFE00_0000`); PL011 `+0x20_1000`, AUX mini-UART
  `+0x21_5000`, mailbox `+0x00_B880`, EMMC2 `+0x34_0000`.
- GIC-400: GICD `0xFF84_1000`, GICC `0xFF84_2000`.
- Boot: firmware loads `kernel8.img` at `0x8_0000`, AArch64, enters at
  EL2, `x0` = DTB pointer; `config.txt` knobs (`arm_64bit=1`,
  `kernel=kernel8.img`, `enable_uart=1`, `armstub`).
- RAM layout for the 1/2/4/8 GiB SKUs and the `>3GiB` aliasing window.

**Done when:** the section exists, links cleanly (`cargo xtask
docs-check`), and is referenced by P1+. No source code changes.

### P1 — Pi-4 boot stub + linker script + production aarch64 kernel binary `[x]`

- Add `kernel/arch/aarch64/link/aarch64-rpi4.ld` (load `0x8_0000`),
  alongside the existing `aarch64-virt.ld`. Two linker scripts is the
  §0.2 boot carve-out, not duplication — they differ only in the origin
  address and a comment.
- Generalise `boot.s` so the EL2→EL1 drop + `.bss` clear + stack setup is
  board-independent (it already is, bar the comment); confirm it works
  from EL2 with the Pi register hand-off. If the Pi enters all 4 cores at
  `_start` (firmware default, no `armstub` spin-table), the stub must park
  secondaries (`MPIDR_EL1` affinity ≠ 0 → `wfe` loop) until PSCI/SMP
  bring-up wants them — fail closed, never race (§2.1).
- Teach `kernel/rustos-kernel/build.rs` + source to build as the
  freestanding **aarch64** production kernel (today `is_freestanding()`
  is hard-coded to `x86_64`). The `freestanding` cfg and the
  boot/panic/serial-sink modules must select the aarch64 boot path and
  linker script by `CARGO_CFG_TARGET_ARCH` — this is build glue, the §17.2
  allow-listed place for target conditionals.
- The binary's `kernel_main(dtb)` wires `Aarch64Arch` into `kernel/core`
  (the single §17.1/§17.2 selection point), mirroring the x86_64 `boot`
  module.

**Done when:** `cargo build -p rustos-kernel --target aarch64-unknown-none`
produces a freestanding ELF that links against `aarch64-rpi4.ld`; a host
unit test covers the new `build.rs` arch/linker selection; no `cfg-check`
/ `deps-check` regressions.

**Landed.** `kernel/arch/aarch64/link/aarch64-rpi4.ld` (origin `0x8_0000`)
sits beside `aarch64-virt.ld`; `boot.s` now parks non-boot CPUs
(`MPIDR_EL1` affinity ≠ 0 → `wfe`) before touching the boot stack, so it
serves both `virt` (PSCI-held secondaries) and the Pi (all-core release).
`kernel/rustos-kernel/build.rs` factors its pure selection logic into
`src/build_support.rs` (host-unit-tested) and emits a build-glue
`kernel_isa` cfg + the per-board linker script — no `cfg(target_arch)` in
the crate body (cfg-check clean). The crate's x86_64 boot pipeline is
gated `kernel_isa="x86_64"`; the new freestanding `boot_aarch64` module +
the aarch64 `kernel_main(dtb)` in `main.rs` construct `Aarch64Arch` (the
§17 selection point), bring up the console, log a boot line, and park
fail-closed. `cargo build -p rustos-kernel --target aarch64-unknown-none`
links a freestanding ELF entered at `0x8_0000`. The discovery-fed
`kernel_core::kernel_main` hand-off (a real memory map / IRQ routing) is
deliberately staged to P2/P3 — fabricating a hardware map would violate
§18.5, and the `-M raspi4b` runtime vertical that proves it cannot pass
until P2's console discovery lands. The `CPACR_EL1.FPEN` enable is now a
single `rustos_arch_aarch64::enable_fp_el1()` helper (§2.2), adopted by
the production binary and the existing aarch64 verticals.

### P2 — Board-discovered UART console (PL011 + mini-UART) `[x]`

- The fixed `serial::PL011_BASE` constant is gone: the console MMIO base +
  register model now live in a new host-testable
  `rustos_arch_aarch64::console` module (an atomic `(base, model)` pair,
  default = the `virt` PL011 base) that the freestanding `serial` sink
  transmits through on every byte. `console::configure_from_fdt` reads the
  base + model from the device tree. The BCM2835 **AUX mini-UART** is a
  second `ConsoleModel` behind the same `rustos_log::Sink` seam (its own
  `AUX_MU_IO`/`AUX_MU_LSR` register offsets + opposite-sense TX-ready bit),
  selected by the `compatible` string — `brcm,bcm2835-aux-uart` vs
  `arm,pl011`. One console abstraction, two register backends (§2.2).
- `platform::FdtDiscovery` emits a `serial`-class `HwNode` carrying the
  discovered UART `compatible` bind key + its `reg` as a capability-gated
  MMIO resource, so the console base is discovered, not assumed.
- `boot_aarch64::boot` calls `console::configure_from_fdt` from the `x0`
  DTB before its first log line (MMU-off-safe: the `lib/fdt` reader is
  byte-wise, no multi-byte Device-memory load — W17).

**Done when:** host unit tests cover the mini-UART/PL011 register
encoders and the `compatible`-string console selection + the discovered
`serial` `HwNode` (against the new `rustos_fdt` `raspi_like_arm` fixture);
and the new `tests/integration/uart_console_qemu_aarch64` vertical boots
the `virt` board, **poisons** the console base, then proves
`configure_from_fdt` overwrites it with the base read from the firmware
device tree and that writes reach that base (it prints two lines over the
*discovered* console before the semihosting PASS finisher). All existing
`virt` aarch64 verticals stay green.

**Emulation gap (honest, not faked — §2.1):** the vertical runs on `-M
virt`, **not** a Pi board, because QEMU's `raspi*` models do not model the
Raspberry Pi GPU-firmware DTB hand-off — they enter an ELF `-kernel` with
`x0 = 0` (verified by GDB on `raspi3b`), and QEMU 8.2.2 has no `raspi4b`
at all. The `virt` board *does* pass its generated tree (which carries a
real `arm,pl011` node), so the runtime discover→configure→print path is
CI-proven there against a genuine firmware tree (the canonical `virt` DTB,
dumped + embedded at build time since `-kernel <ELF>` passes no pointer).
The Pi's *specific* console base + the mini-UART register layout are
covered by the host unit tests against the `raspi_like_arm` fixture, and
printing on real Pi PL011 silicon is an on-metal acceptance item for the
Arc C peripheral stages (where the real firmware populates `x0`).

### P3 — GIC-400 from the tree + Pi RAM map `[x]`

- The GICv2 driver register layout already matches GIC-400; thread the
  GICD/GICC bases from `FdtDiscovery` instead of the `virt` constants
  `gic::{GICD_BASE,GICC_BASE}`. Emit a GIC `HwNode` from discovery.
- Generalise the early memory map: `FdtDiscovery::first_memory_region`
  already reads `/memory`; confirm it yields the Pi's `0x0`-based RAM and
  feed it to `kernel/mem` so the allocator/page tables cover real Pi RAM,
  not the `virt` `0x4000_0000` assumption.

**Done when:** the existing `ipi_smp` / `sched_drive` aarch64 verticals
(or a new `-M raspi4b` analogue) run over **discovered** GIC bases and
Pi RAM, GICv2 IRQs + SGIs deliver, and `cargo xtask cfg-check` confirms no
board constants leaked outside the arch crate.

**Landed.** The fixed `gic::{GICD_BASE,GICC_BASE}` constants are gone:
`gic` now holds the active `(gicd, gicc)` pair as an atomic (default = the
`virt` GICv2 `0x0800_0000`/`0x0801_0000`) that the freestanding
`VolatileGicMmio` reads on every access, with `gic::find_gic` /
`configure_from_fdt` over `lib/fdt` selecting the first GICv2-class
controller (`arm,gic-400`, `arm,cortex-a15-gic`, …) and reading its two
`reg` regions. `platform::FdtDiscovery` emits an `InterruptController`
`HwNode` carrying the discovered `compatible` bind key + both register
windows (`HwDeviceClass::InterruptController` already existed — no ABI
change). The `lib/fdt` `virt_like_arm` / `raspi_like_arm` fixtures grew a
GIC node (virt `arm,cortex-a15-gic`; Pi `arm,gic-400` @ `0xFF84_1000`/
`0xFF84_2000`); host tests cover the GIC discovery, the `HwNode`, and the
fail-closed no-GIC path. `boot_aarch64` parses the `x0` DTB once and
points the console **and** the GIC driver at their discovered bases and
reads the `/memory` window, logging `gic_discovered` / `ram_discovered`
(the live allocator + `kernel_core::kernel_main` hand-off over that map is
deliberately staged to P4/P6 — a hard-coded map would violate §18.5).
**Runtime proof on `-M virt`:** the `ipi_smp_qemu_aarch64` vertical now
**poisons** the GIC base, rediscovers it from the embedded `virt` DTB
before `gic::init`, and asserts it moved to the `virt` GICv2 base, so the
delivered IPI exercises the *discovered* base (`irq_qemu_aarch64` likewise
reads `gic::current()`); both PASS. `cargo xtask cfg-check` stays clean
(no board constant leaked outside the arch crate). The Pi's specific
GIC-400 bases are host-unit-tested + an on-metal item (no `raspi4b` in
QEMU — the same gap as P2).

### P4 — Generic timer + live scheduler on the Pi `[x]`

- Reuse the W7 live-scheduler wiring (`preempt` + `context` + `mlfq`) over
  the discovered GIC-400 + Pi generic-timer PPI. The Pi's `CNTFRQ_EL0` and
  timer PPIs come from the tree (`timer_ppi` already reads them); confirm
  the Pi's 54 MHz crystal / `CNTFRQ` is honoured rather than the `virt`
  value.

**Done when:** a `-M raspi4b` `sched_drive` vertical drives the live
`Scheduler` ≥ 20 timer ticks + ≥ 1 IPI tick, exactly as the `virt` one
does (`plans/WIRING.md` W7).

**Landed.** The generic-timer counter rate is now a *discovered* board
fact rather than the raw register: `fdt::timer_clock_frequency` reads the
`/timer` node's optional `clock-frequency` override (the standard
`arm,armv?-timer` binding the firmware carries when `CNTFRQ_EL0` is
mis-programmed) and the pure, host-tested `fdt::effective_timer_hz`
selects it over `CNTFRQ_EL0` when present and non-zero, else falls back to
the register (a zero override is treated as absent — never a 0 Hz timer,
§2.9). The freestanding `kernel_arch::timer_frequency_hz(&fdt)` composes
the two; `boot_aarch64` seeds the `Aarch64Arch` clock/preempt interval
from it and logs `timer_hz_from_tree`. `timer_clock_frequency` matches the
timer node through the shared `Fdt::nodes` early-returning walk (the same
byte-safe traversal `gic::configure_from_fdt` uses, §2.2) — **not** the
whole-tree `Fdt::property`/`walk` scan, which faults under the verticals'
MMU-off boot when the compiler widens the byte reads; reaching only the
matched node's own properties keeps discovery safe MMU-off. **Runtime
proof on `-M virt`:** the `sched_drive_qemu_aarch64` vertical now derives
the tick interval from `timer_frequency_hz(&fdt)` over the embedded `virt`
DTB and **poisons** the GIC base, rediscovering it (`configure_from_fdt`)
before `gic::init`, so the ≥ 20 timer ticks + ≥ 1 IPI that drive the live
`Scheduler` run over the *discovered* GIC base and frequency. The `virt`
tree omits `clock-frequency`, so the runtime path exercises the register
fallback while the override branch is host-unit-tested; honouring the Pi's
real 54 MHz crystal is an on-metal item (no `-M raspi4b` in QEMU — the
same gap as P2/P3). `cargo xtask cfg-check` stays clean.

### P5 — SMP bring-up on the Pi (PSCI vs spin-table) `[x]`

- The Pi 4 firmware exposes PSCI when `armstub8.bin` is present (default
  on current firmware); the W6 `psci::CPU_ON` path then works unchanged.
  Confirm the conduit (`smc` vs `hvc`) is the one `fdt::psci_method`
  discovers on the Pi tree (Pi uses `smc`). If a target firmware lacks
  PSCI, the honest options are documented (require the PSCI armstub, or
  build the carried-forward spin-table bring-up as a *separate, tested*
  port-side path — never untested asm, §2.1).

**Done when:** a `-M raspi4b --cpus 4` vertical starts the secondary
cores via the discovered conduit and delivers a directed SGI, mirroring
`ipi_smp_qemu_aarch64`; the conduit choice is discovered, not assumed.

**Landed.** The PSCI conduit is now a *discovered* board fact end to end.
`fdt::psci_method` was moved off the whole-tree `Fdt::property` scan onto
the shared `Fdt::nodes` early-return walk (matching the `/psci` node by an
`arm,psci` `compatible` prefix and reading `method` from that node only),
the same byte-safe traversal `gic::configure_from_fdt` /
`fdt::timer_clock_frequency` use (§2.2) — so conduit discovery is safe on
the MMU-off bring-up path where a full-tree scan faults (the P4
watch-out). `boot_aarch64` now reads the conduit from the `x0` DTB,
installs it via `Aarch64Arch::with_psci_method`, and logs
`psci_conduit_discovered`; a tree with no `/psci` node leaves the conduit
unset, so the `SecondaryBringup` HAL fails closed (`SmpError::NotReady`)
rather than assuming one (§5.4.5). **Runtime proof on `-M virt`:**
`ipi_smp_qemu_aarch64` now *discovers* the conduit from the embedded
`virt` tree (replacing the former hard-coded `VIRT_PSCI_METHOD`), asserts
it is the board's `hvc`, fails closed otherwise, and starts the secondary
core + delivers a directed SGI over *that* discovered conduit. Host tests
cover the conduit read from both the `virt` (`hvc`) and `raspi` (`smc`)
fixtures and the fail-closed no-`/psci` path. The Pi's `smc` conduit (via
`armstub8.bin`) flows through the identical path and is an on-metal
acceptance item (no `-M raspi4b` in QEMU — the same gap as P2/P3/P4).
`cargo xtask cfg-check` stays clean.

### P6 — Spawn `init` into EL0 on the Pi `[ ]`

- Wire the freestanding aarch64 `rustos-kernel` boot path through to
  bringing up `kernel/{mem,ipc,sec,syscall}`, mounting a root, and
  spawning PID 1 (`userland/system/init`) into EL0 via the existing
  `userentry` `eret` path — the user-mode milestone the issue calls out.

**Done when:** under `-M raspi4b`, the kernel reaches `init` in EL0 and
`init` emits its first log line over the discovered UART; a vertical
asserts the EL0 transition + the `init` banner. (This is the
"boot into user mode" milestone.)

### P7 — VideoCore mailbox + framebuffer (metal) `[ ]`

- Implement the BCM2711 **mailbox** property-channel interface (a small
  `drivers/`-side service or `lib/` helper) to query the firmware
  framebuffer (set physical/virtual size, depth, get the framebuffer bus
  address + pitch). Translate the VC bus address to an ARM physical
  address and map it through a capability (§4, no ambient authority).
- Feed the resulting `HvsConfig` to the existing
  `drivers/display/rpi_hvs` driver and add its first **hardware vertical**
  (mailbox-mocked under emulation; real HDMI capture on metal).

**Done when:** the mailbox property protocol has host unit tests
(request/response framing, bus↔physical translation, fail-closed on a bad
aperture), `rpi_hvs` consumes a discovered `HvsConfig`, and a metal
bring-up checklist + a captured "framebuffer cleared to theme colour"
photo/UART-log is recorded as the acceptance artefact.

### P8 — SD-card storage (EMMC2) `[ ]`

- A `drivers/storage` EMMC2 (Arasan/SDHCI-derived) driver for the Pi 4 SD
  host, bound by `devmgr` against its `compatible` string (§18.3). Read
  path first (mount the root the installer laid down), then write.

**Done when:** host unit tests cover the SDHCI command/response + block
transfer state machine against a mock host; a metal checklist demonstrates
reading the FAT boot partition and the RustFS root from a real card.

### P9 — Bootable SD image (`tools/mkimage`) `[ ]`

- Build `tools/mkimage` (Stage 8 dependency) to emit
  `images/rustos-aarch64-rpi.img`: a FAT32 boot partition carrying the Pi
  firmware blobs (`start4.elf`, `fixup4.dat`, `bcm2711-rpi-4-b.dtb`,
  `armstub8.bin`), `config.txt`, and the `kernel8.img` from P1, plus the
  RustFS/secure-layout root partition from the §11 installer defaults.
  Pure Rust + audited wrappers only (§12, no shelling to `parted`/`mkfs`).
- Firmware blobs are third-party redistributables: pin + checksum them per
  §19.3; document their provenance. They are **not** committed to the repo
  unless licensing + supply-chain review (`cargo deny`, §7) clears it —
  otherwise `mkimage` fetches them from a pinned, checksummed source as a
  build input (never a post-install network fetch, §19.3).

**Done when:** `cargo xtask build --target aarch64-rpi` (and `--headless`)
produces a flashable `.img`; `docs/src/install/raspberry_pi.md` documents
flashing + first boot; the image boots P6 (user mode) on real hardware per
a recorded checklist.

### P10 — USB-HID input + desktop on the Pi `[ ]`

- Bring up the Pi 4 USB host (VL805 PCIe → xHCI for the USB-A ports, and
  the DWC2 OTG) far enough to enumerate a USB-HID keyboard + mouse under
  `drivers/bus/usb` + `drivers/input`, so the WM input router has real
  events.
- Run `userland/gui/{wm,taskbar,session}` on the HVS path: the headless
  build stays first-class (§17.3), and the graphical session is the
  launchable option `userland/session/login` offers when the display +
  input drivers loaded.

**Done when:** on real hardware the desktop composites through `rpi_hvs`,
the taskbar renders, and a USB keyboard/mouse drives the WM; a recorded
demo (photo + UART log) is the acceptance artefact. Headless `-M raspi4b`
CI stays green throughout.

---

## 4. Cross-cutting requirements (apply to every stage)

- **Discovery, not constants.** Any new MMIO base lands as a discovered
  `hwtree` resource, never a fresh `const PI_*_BASE`. `cargo xtask
  cfg-check` must stay clean; the grandfather list stays empty.
- **Capabilities + audit.** Every new device handle (UART, GIC, mailbox,
  framebuffer, SD, USB) is reached through a capability the matched
  `hwtree` node requested; every match/load/skip is logged with a stable
  event id (§5.4 / §18.3).
- **Honest emulation gaps.** Where `raspi4b` cannot model a peripheral
  (HVS, real HDMI, real USB timing), say so in the stage and carry the
  metal checklist; never fake a passing vertical (§2.1 / `WIRING.md` §0.4).
- **`virt` stays green.** None of this regresses the QEMU `virt` board:
  the board difference is discovered data, so the same code serves both.

## 5. Definition of done (per stage, run over the whole project)

```
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all && cargo fmt --all --check
cargo xtask ci            # clippy -D warnings, deps-check, cfg-check, test matrix,
                          # docs-check, deny, c-header drift, proptest/fuzz --quick,
                          # model-check, spec-review, abi-check
cargo xtask fuzz --secs 5
tools/ci/soak.sh both --secs 10
```

The QEMU verticals are **not** in the host-only `cargo xtask ci` gate; run
the enrolled matrix separately — and add a `-M raspi4b` invocation path to
`tools/qemu/src/aarch64.rs` so the new board verticals run:

```
cargo xtask test --qemu
```

A single Pi-board QEMU bin can be iterated directly once the `raspi4b`
machine support lands in the runner:

```
cargo build -p <pkg> --target aarch64-unknown-none
cargo run -q -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/aarch64-unknown-none/debug/<bin> --arch aarch64 \
    --board raspi4b --timeout-secs 60
```

Stages that can only be proven on metal (P7–P10) additionally record a
hardware bring-up checklist + a UART-log / photo acceptance artefact under
the stage, since CI has no Pi attached.
