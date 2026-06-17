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

9. **Metal re-verification is guaranteed, not speculative (operator
   commitment, binding).** The operator re-verifies every applicable
   stage on a real Raspberry Pi 4B as the work proceeds and supplies the
   UART/debug-log (or photo) acceptance artefact between chunks. The
   metal-acceptance step *will* be checked every time it applies — it is
   never assumed, skipped, or treated as optional. Therefore design
   decisions are made for the **most correct, properly-designed,
   senior-review-clean** outcome (`AGENTS.md` §2.6 / §23): security and
   correctness are the floor (§2.1 / §5.4 / §23.1), performance is
   first-class (§2.16), and drivers are generic, modular hardware
   interfaces — from PCI/PCIe bridges down to the individual USB-HID
   interface (keyboard, mouse, storage, scanner, printer, …) and other
   PCI devices (storage, serial, parallel/printer ports, …) — never a
   Pi-4-only special case (the work must equally serve `x86_64`,
   `riscv64`, and other boards: §0.2 / §17.2 / §18). Do **not** trade
   design quality for a smaller blast radius on the assumption metal
   verification is optional: it is not. A chunk touching a live,
   metal-confirmed path lands host-tested **plus** a metal checklist —
   never a hack to dodge a check (§2.1).

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
| Boot protocol | QEMU `-kernel <elf>`, aarch64 hand-off `x0 = DTB` at EL2/EL1 | Pi firmware (`start4.elf`) loads `kernel8.img` at `0x80000`, enters EL2, `x0 = DTB` (Pi firmware-supplied) |
| Load address / linker | `0x4020_0000` (`aarch64-virt.ld`) | `0x8_0000` (needs an `aarch64-rpi4.ld`) |
| Console UART | PL011 @ `0x0900_0000` (fixed const) | PL011 @ `0xFE20_1000` *or* mini-UART (AUX) @ `0xFE21_5040`, base discovered |
| Interrupt controller | GICv2 @ GICD `0x0800_0000` / GICC `0x0801_0000` (fixed const) | GIC-400 @ GICD `0xFF84_1000` / GICC `0xFF84_2000`, base discovered |
| RAM base | `0x4000_0000` | `0x0` (low 1 GiB; up to 8 GiB with the `>3GiB` window) |
| Display | virtio-gpu / ramfb | VideoCore mailbox framebuffer → `drivers/display/rpi_hvs` (HVS) |
| Storage | virtio-blk-mmio | EMMC2 SD host controller (`drivers/storage`) |
| Input | virtio-keyboard-mmio | USB HID via the VL805/DWC2 USB host (`drivers/bus/usb`) |
| Image builder | `tools/mkimage` emits `images/rustos-aarch64-rpi.img` (P9) | flash + boot the emitted image on metal |

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
- **Arc D — Multi-user login (P11).** Every text console sits at a
  `login:` prompt backed by the `/System/Security/Users` database; the
  video and UART consoles are separate session contexts.

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
Both EL1 entry trampolines (`boot.s` `.Lin_el1`, `smp.s`) write the known
MMU-off `SCTLR_EL1` (`paging::SCTLR_MMU_OFF`, ARMv8.0 RES1 bits only,
unit-test-pinned) before the first EL1 data access, and
`AddressSpace::switch` installs the whole known `paging::SCTLR_MMU_ON`
(RES1 + M + C + I, after `ic iallu`) rather than OR-ing `M` into the
live register: `SCTLR_EL1` is architecturally UNKNOWN at first EL1 entry
on real silicon (EL2 hand-off and PSCI `CPU_ON` alike), and a carried
UNKNOWN `WXN`/`EE` bit hung the metal Pi 4 at the MMU switch while QEMU
(benign reset values) stayed green.
The same UNKNOWN-reset-state rule holds one level up: the Pi firmware
stub sets only `SCTLR_EL2` and `CPUECTLR_EL1.SMPEN`, so `boot.s`'s EL2
path writes every EL2 control register **whole** with the
unit-test-pinned hand-off values in `rustos_arch_aarch64::el2`
(`HCR_EL2 = RW`, `CNTHCTL_EL2 = EL1PCTEN|EL1PCEN`, `CPTR_EL2 =` RES1,
`MDCR_EL2 = 0`, `VPIDR/VMPIDR` mirrored) — an UNKNOWN `HCR_EL2.TVM`
traps EL1's first `MAIR/TCR/TTBR/SCTLR` write into vector-less EL2,
hanging the metal Pi 4B silently at the MMU switch while QEMU stayed
green.
The boot identity map is bounded to backed memory: gigapages are mapped
only when named by the configured Device mask or the configured RAM mask
(`paging::configure_ram_gigapages` / `identity_ram_mask`, default all —
the historic map — for host tests and the QEMU integration kernels), and
every other L1 slot is left *invalid* so speculation cannot reach
unbacked bus windows (the metal Pi 4B wedged at the MMU switch exactly
there while QEMU stayed green). The boot path derives the RAM mask
pre-MMU from the kernel image extent, the firmware DTB blob, and the
scan-out surface, then widens it with the post-MMU-discovered `/memory`
window — re-installing the mask for later process spaces and extending
the live boot space via `AddressSpace::ensure_identity_gigapage`
(invalid→valid, store barrier only). The switch itself is
real-silicon-honest: the just-written tables are swept to PoC
(`PageTablePool::clean_invalidate_to_poc`, `dc civac` per
`CTR_EL0`-decoded line — MMU-off stores bypass the cache but the walker
reads back cacheable, so firmware cache residue would shadow the
descriptors) and `switch` orders those Device-nGnRnE stores with a
full-system `dsb sy` before enabling translation. The pool's allocation
counter is translation-aware: MMU-off it advances by plain load + store
(LDXR/STXR exclusives never succeed on the BCM2711's MMU-off
Device-nGnRnE accesses, so a `fetch_add` spins forever on metal while
QEMU stays green; MMU-off allocation is pre-SMP boot-CPU-only by
construction) and reverts to `fetch_add` once translation is live.
With those fixes the metal Pi 4B (8 GB) boots the production pipeline
end to end **through user space**: the
stage-p1 boot line, the kernel-core phase log, PID 1 `init`'s EL0 entry
and banner, and the spawn/wait/exit supervision cycle all render on
both UART0 and the HDMI console (every spawned space's identity window
is derived from the configured Device/RAM gigapage masks,
`paging::configured_identity_gigapages` — P6c-3/P6d — since the former
hard-coded 2 GiB `virt` window dropped the Pi's gigapage-3 UART/GIC
from PID 1's root). The session formerly read end-of-input at its first
prompt (the metal had nothing queued in the PL011 RX FIFO) and exited,
exhausting `init`'s crash-loop budget; the kernel-core
`BlockingConsoleRead` backing (P6e-2) now parks the reader until UART
RX delivers bytes, so the metal session waits at `rustos$ ` for the
user to type.
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
  base + model from the device tree, decoding the node's `reg` with its
  parent bus's cell counts and translating it through the ancestor
  buses' `ranges` (the shared `fdt::scan_translated` /
  `fdt::translated_reg` machinery, §2.2) — the real Pi tree's UARTs sit
  under `/soc` with one-cell *bus* `reg` values (`0x7E20_1000`) remapped
  to CPU-physical space (`0xFE20_1000`); an untranslatable node is
  skipped, never poked at its raw bus address (§2.9). The
  `raspi_like_arm` fixture mirrors that real shape (root 2+1 cells,
  `/soc` simple-bus with 1+1 cells and the three BCM2711 `ranges`,
  bus-address parameters). The BCM2835 **AUX mini-UART** is a
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
- Discovery alone leaves real silicon silent: `uart_init::init_from_fdt`
  runs right after it, muxing GPIO 14/15 to the PL011 (`GPFSEL1` ALT0 +
  pull-none, gated on a discovered `brcm,bcm2711-gpio` node) and
  programming the PL011 line (TRM order, 9600 8N1 + FIFOs from the
  `config.txt`-pinned 48 MHz `init_uart_clock`) — QEMU's powered-up
  PL011 masked the omission; the metal Pi 4B booted with a permanently
  silent UART0 without it. Pure, host-tested register arithmetic; the
  freestanding layer is volatile MMIO only (§2.2).
- The real firmware tree is a regression input: the
  `real_dtb_probe` integration test runs the production discovery walks
  (console, GPIO, mailbox, memory) over the pinned
  `bcm2711-rpi-4-b.dtb` when the firmware cache is present (skips
  honestly when not fetched).

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
controller (`arm,gic-400`, `arm,cortex-a15-gic`, …) and reading its
first two `reg` regions (GICD, GICC), each decoded with the parent
bus's cell counts and translated through the ancestor buses' `ranges`
(`fdt::translated_reg`) — the real Pi tree's GIC-400 sits under `/soc`
with one-cell bus `reg` values (`0x4004_1000` → `0xFF84_1000`).
`platform::FdtDiscovery` emits an `InterruptController` `HwNode`
carrying the discovered `compatible` bind key + every register window
(`HwDeviceClass::InterruptController` already existed — no ABI change).
The `lib/fdt` `virt_like_arm` / `raspi_like_arm` fixtures carry a GIC
node (virt `arm,cortex-a15-gic` at the root; Pi `arm,gic-400` under
`/soc` with the real four bus-address regions); host tests cover the
GIC discovery, the `HwNode`, and the fail-closed no-GIC path.
`boot_aarch64` parses the `x0` DTB and points the console **and** the
GIC driver at their discovered bases MMU-off, then reads the `/memory`
window once the MMU is on, logging `gic_discovered` / `ram_discovered`
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
from it and logs `timer_hz_from_tree` plus the resolved rate itself as
`timer_hz_hex`. That rate also drives every `kernel_arch::busy_delay_us`
settle the in-kernel bring-up uses. The metal capture read
`timer_hz_from_tree=false timer_hz_hex=0x337_f980` — exactly the Pi 4's
54 MHz crystal — so `CNTFRQ_EL0` is correctly programmed and a
mis-programmed-rate over-wait is **ruled out** as the cause of the P10
multi-second USB bring-up pause. The `4116` "bring-up delay timing
measurement" (`keyboard_service` brackets the whole chain with
`kernel_arch::read_cntpct` and tallies its `GenericTimerDelay`'s requested
microseconds) read `requested_us_hex≈356 ms` (259 delay calls) yet
`counter_elapsed_us_hex≈14.3 s` at the correct 54 MHz — so ≈14 s of real
time elapsed with only ≈356 ms in `busy_delay_us`: the counter is sound and
the seconds are code-side, but `4116` alone cannot split *where* they go.
Two diagnostics localise it: `SerialSink` prefixes every line with a
monotonic `CNTPCT_EL0`-derived `[t=<ms>ms]` stamp (`kernel_arch::uptime_ms`,
so a capture reads the real wall time between any two lines), and `build.rs`
emits `KERNEL_BUILD_ID` (git short hash + `+dirty` + `SOURCE_DATE_EPOCH`-aware
build epoch, §19.3), logged as `build_id` on the `4097` line so a capture
proves which build is running. The timestamped capture (with `build_id`
confirming the current image) was **decisive** and corrected the earlier
un-timestamped guess: the caps-readiness wait (`4108`→`4109`) is only
~0.35 s — the `wait_for_caps_ready` *elapsed-wall-time* bound
(`CAPS_READY_BUDGET_US`≈256 ms via `Delay::now_us`, retained as a §2.16
defence) works, and `4109 polls_hex=0x100` is 256 *fast* reads (the BCM2711
master-abort returns the `dead_dead` poison in ~1.3 ms, not the ~54 ms first
inferred) — so the caps loop is **not** the pause. The ~14 s is almost
entirely inside `BrcmPcieRc::bring_up`: ~11.2 s between the RC-register-window
map (`4105 phys_base=fd50_0000`) and `4101 link trained`, with no log line
between, while bring_up's coded delays total only ~hundreds of ms and its
reads target the RC's own (fast) register block. The `4117` per-phase split
(`BringUpTiming` from the `Delay` clock, host-tested) then pinned the ~11 s
to the reset phase, then the reset sub-spans pinned it to the **first access
to the MISC register block** (`0x4xxx`). Early experiments that powered the SerDes or read link status before
cycling the bridge reset stalled identically — *every* early MISC
access master-aborts — refuting the SerDes-IDDQ theory. The
real gate is the controller reset — the BCM2711 holds the core off until the
always-accessible RGR1 bridge `sw_init` reset (`0x9210`) is cycled, which is
why the **same** `MISC_PCIE_STATUS` read costs ~8 µs in the config phase (the
reset having run by then) yet ~10.8 s before it. The BCM2711 PCIe bring-up sequence
never touches a MISC register before cycling the bridge reset.
**Fixed (host-proven; confirmed gone on metal):**
`BrcmPcieRc::reset_controller` cycles **only** the always-accessible RGR1
bridge `sw_init` reset (`0x9210`) — bringing the core and its MISC block
online — then lets the core and MISC block settle; the
`4117` capture confirmed `reset_swinit_us`/`reset_settle_us` collapse to
microseconds and the ~14 s pause is gone. The gentlest no-touch-probe
bring-up does **not** re-assert a fundamental reset or toggle the SerDes
`IDDQ` (either could drop the VL805 firmware the previous boot stage
loaded over the power-on link); `PERST#` is left as the handoff left it
and `train_link` deasserts it (the single `PERST#`-deassert edge that
re-triggers any `VideoCore` VL805 firmware reload
(see the firmware item below). (The Pi UART is the SoC's
own PL011/mini-UART — no path to the PCIe/VL805 — so logging cannot perturb
the controller.) `timer_clock_frequency` matches the
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

### P6 — Spawn `init` into EL0 on the Pi `[x]`

The user-mode milestone is the first time the *production* kernel reaches
EL0 on any arch (today only per-test fixtures do, via `spawn_and_enter`),
and the standing direction is to do it *properly*: `init` is a real Rust
program that parses a startup config and writes its first line to the
**console** — the detected framebuffer if any, else the first discovered
UART. That console line needs a real syscall (there was none), and
`init` "starting the user in a shell" needs a userland process-spawn
syscall (there is none). So P6 is staged into chunks, each landed green
on its own before the next.

- **P6a — `console_write` `abi-v1` syscall `[x]`.** New syscall number 11
  + `CAP_CONSOLE_WRITE` (id 18), the `SyscallSpec` row, the
  `kernel/syscall` dispatch arm + recomputed `SYSCALL_TABLE_HASH`, the
  `kernel/core` `ConsoleWrite` seam (boot installs the device — framebuffer
  else first UART — defaulting to a fail-closed `NULL_CONSOLE` →
  `NotImplemented`) + the copy-in handler, the `ros_sys_console_write`
  C stub, and the regenerated C header. `SYSCALL_NAME_MAX` bumped 12→13
  to fit the name. All host tests + `abi-check` + `c-header` green.
- **P6b — `rustos-init` becomes a real program `[x]`.** The `rustos-init`
  package now builds the `init` bundle's `Run` entry-point binary
  (`src/run.rs`, `AGENTS.md` §16.5) as a **pure-Rust** program. RustOS is
  Rust-only (`AGENTS.md` §1), so it links the new pure-Rust userland runtime
  `lib/rt` (`rustos-rt`) — **never** the C ABI (`crt0` + `abi-sys`), which
  exists solely for non-Rust programs (`AGENTS.md` §16.4). `rustos-rt`
  provides `_start`, the §19.2 stack canary, the panic handler, and idiomatic
  syscall wrappers; `rustos_rt::entry!` names the program's safe `main() ->
  i32`, which parses the compiled-in startup config and writes its first
  banner line through the P6a `console_write` syscall, the runtime routing the
  return through `exit`. Both `rustos-rt` and the C ABI reach the kernel
  through the **one** shared trap primitive `lib/abi-trap` (`rustos-abi-trap`,
  the §1 syscall/svc/ecall carve-out), so the trap assembly is not duplicated
  (`AGENTS.md` §2.2). The **startup config** (`src/startup.rs`) is a tiny,
  allocation-free, host-tested text format with two required directives —
  `console` and `session <absolute-path>` — that fails closed on an
  unknown/duplicated directive, a wrong/missing argument, a non-absolute
  path, an over-long config, or an omitted directive (`AGENTS.md` §2.9,
  §19.5). The program links **only** the runtime and its own parser — never
  the orchestrator library, whose `alloc`+crypto chain would be §2.3 bloat —
  so the parser lives beside the binary and the shipped image carries no
  crypto and **no `unsafe`** (verified: the linked aarch64 PIE exports
  `_start`/`__rustos_rt_main`/`rust_rt_start` and the mangled
  `rustos_rt::console_write`, with zero `ros_sys_*` and zero crypto symbols).
  A self-contained `build.rs` sets the `freestanding` cfg from `target_os`
  only (no `target_arch`, so `cfg-check` stays clean; no dependency on the
  tests harness, so §17.4 layering stays clean); `Run.ld` mirrors the proven
  PIE link layout the userland runtimes share. Host DoD green (3 new
  `rustos-rt` tests + 10 startup-config tests + the freestanding aarch64 build
  via `build-std=core,alloc,compiler_builtins`). The end-to-end EL0 spawn of
  this binary is P6c.
- **P6c — production boot reaches EL0 `[x]`.** Wire the freestanding
  aarch64 `rustos-kernel` boot path through `kernel/{mem,ipc,sec,syscall}`
  over the discovered `/memory` map, install the console (discovered
  framebuffer else first UART) via `with_console`, embed the `init` rxe,
  and spawn PID 1 into EL0 via the `userentry` `eret` path. Add a `-M virt`
  vertical asserting the EL0 transition + the `init` banner on the
  discovered UART. This is the largest P6 step — it stands up the aarch64
  production runtime *and* the OS's first production-boot→EL0 spawn (no arch
  reaches user mode from `kernel_main` today; `kernel_core::kernel_main`
  halts after `BootCompleted`) — so it is landed in sub-increments, each
  green over the whole project on its own:
  - **P6c-1 — discovered `/memory` → `BootMemoryMap` `[x]`.** The aarch64
    boot path turns the firmware-discovered RAM window + the linker
    `__kernel_end` into the canonical two-region physical map the live
    allocator hand-off consumes (reserve `[ram_base, __kernel_end)`,
    page-align the rest usable), the riscv64 boot pipeline's
    `build_memory_map` analogue. The bounds/overflow arithmetic lives in a
    host-tested `rustos_kernel::mem_map` module (8 unit tests; gated to the
    aarch64 build and `cargo test` so it is never dead code, §2.3) because
    the bare-metal `boot_aarch64` cannot be host-compiled. The boot audit
    line now records `mem_map_built` / `mem_map_status` /
    `usable_bytes_hex` / `reserved_bytes_hex`, failing closed to a status
    string (never a panic, §2.9) on an absent or malformed window.
  - **P6c-2 — MMU + `kernel_main` hand-off `[x]`.** `boot_aarch64`
    discovers the console + GIC bases MMU-off (early-return walks),
    derives the identity map's Device gigapage mask from them
    (`paging::identity_device_mask` — discovered MMIO gigapages Device,
    the kernel image's own gigapages Normal/executable; `virt`: GiB 0
    Device, Pi 4: GiB 3 Device with the kernel at `0x8_0000` in a Normal
    GiB 0), installs it via `paging::configure_device_gigapages`, then
    enables the stage-1 identity MMU
    (`AddressSpace::new_identity_gigapages` over a static boot
    `PageTablePool`, 512×1 GiB, then `switch`) and installs the EL1
    vectors *before* any further work, so the `kernel_core`
    allocator/scheduler atomics run on Normal memory and the full-tree
    `first_memory_region` FDT walk is MMU-on-safe (the §watch-out
    hazard). The boot audit line records `device_gigapages_hex`
    (`virt`: `0x1`; Pi 4: `0x8`). It adds the local `Aarch64BinArch`
    `KernelArch` wrapper (orphan-rule sibling of the x86_64 `BinArch` /
    riscv64 `RiscvBinArch`), a slot-based aarch64 `production_dispatch` +
    `DISPATCH_SLOT` (the shared arch-neutral frame-read / errno-encode /
    slot-forward logic factored into a host-tested `dispatch_core` module,
    §2.2), a `UartConsole` `ConsoleWrite` over the discovered UART
    (`serial::write_console_bytes`), and hands a validated `BootInfo`
    (`.with_console(&UART_CONSOLE)`) to `kernel_core::kernel_main` — so the
    aarch64 production kernel reaches `BootCompleted` like x86_64/riscv64,
    or parks fail-closed (§2.9) on an unsound hand-off. `kernel/core` grew
    a `BootInfo.console` field + `with_console` builder (default
    fail-closed `NULL_CONSOLE`) threaded into `KernelDispatchHook::new`. The
    `kernel_arch_boot_aarch64` vertical now boots the *production* pipeline
    to `AuditEvent::BootCompleted` on `-M virt` (embedded `virt` DTB; QEMU
    passes no `x0`). `boot.rs::main` passes `&SERIAL_SINK` as both sinks.
  - **P6c-3 — embed `init` rxe + spawn PID 1 into EL0 `[x]`.** The
    `rustos-kernel` build script compiles the pure-Rust `rustos-init-run`
    `Run` binary PIE (its own `Run.ld`) and converts the linked ELF to an
    embedded `rxe` blob with `rustos_itest_harness::elf2rxe` (stamped with
    the kernel's `SYSCALL_TABLE_HASH`, biased to 64 GiB) — the same path the
    cc3/spawn fixtures use (§2.2; host-only build glue, RustOS stays
    Rust-only §1). `kernel/core` gained an arch-neutral PID-1 spawn seam:
    `BootInfo.init: Option<&dyn InitSpawn>` + `with_init`, invoked by
    `kernel_main` after `BootCompleted`; the object-safe `InitSpawnCtx`
    (`frames`/`audit`/`admit_init`) lets the arch seam build the image
    (`spawn_image` — the new authorise+build+`ProcessSpawned`-audit half of
    `spawn_and_enter`, no enter) while the core registers PID 1 with the
    scheduler + capability table and dispatches it. The aarch64
    `init_spawn` seam builds an identity user address space whose window
    is derived from the configured Device/RAM gigapage masks
    (`paging::configured_identity_gigapages` — 2 GiB on `virt`, 4 GiB on
    the Pi 4, whose UART/GIC live in gigapage 3; the 64 GiB bias avoids
    the gigapage collision), parses the embedded `rxe`, and
    boxes the `userentry` `eret` as the scheduler task body; the body runs
    under `step` so the per-CPU `current_task` is set when `init`'s first
    `svc` traps back. PID 1 runs as uid 0 with `{CAP_CONSOLE_WRITE}`. The
    `spawn_init_qemu_aarch64` `-M virt` vertical asserts the EL0 transition:
    `ProcessSpawned` (4030) → `SyscallInvoked` (5000, `init`'s audited
    `exit`) → semihosting PASS. **Locking fix:** the production
    `KernelDispatchHook` now snapshots the caller's caps and drops the read
    guard before dispatch, so the caps-mutating handlers (`exit`,
    `cap_delegate`, `cap_revoke` — all take `caps.write()`) no longer
    self-deadlock the writer-preference `RwLock` (a latent bug, since no
    arch reached EL0 through the real hook before).
  - **P6c-3 follow-up — registry-storable address space; the `init` banner
    prints `[x]`.** An arch `AddressSpace<P>` is `!Sync` (owns a `&'static
    mut` root + a non-`Sync` page-table source), so it could not be stored
    in the `Send+Sync`, lock-shared `AddressSpaceRegistry`, and PID 1's
    `console_write` user-copy resolved no address space and failed closed
    with `BadAddress`. Added `AddressSpace::freeze()` → `FrozenAddressSpace`
    in `kernel/mem`: a `Send+Sync` POD snapshot walking every live page
    through `translate` into a `BTreeMap<Page,(Frame,MapFlags)>`, so it
    answers the copy path's permission checks identically to the live space.
    `InitSpawnCtx::admit_init` now also takes the boxed frozen view + boxed
    `DirectPhysMap`; `KernelInitSpawner` registers them under
    `SecTaskId(task_id)` in `&state.aspaces` (fail-closed on a duplicate
    id), and the aarch64 `init_spawn` seam freezes `space` after
    `spawn_image` and passes both. `init`'s `run.rs` now gates its `exit` on
    a full-length `console_write` (parks fail-closed otherwise, §2.9), so
    the `spawn_init_qemu_aarch64` vertical's PASS (keyed on the audited
    `exit` 5000 — `console_write` is `audit:false`) now genuinely proves the
    banner reached the console (`-M virt`, verified green).
- **P6d — userland process-spawn syscall `[x]`.** Add the `abi-v1`
  spawn syscall (gated by `CAP_PROC_SPAWN`, already reserved at id 17) +
  an embedded-program registry so `init` can launch a separate process.
  Per the standing direction this is being done *properly* — a real
  concurrent process (its own isolated address space, scheduled
  independently), not an `exec`-style hand-off — which requires real
  kernel-thread EL0↔EL0 context switching the kernel does not have yet.
  So P6d is itself staged in **`plans/SPAWN.md`** (SP0 design; SP1
  kernel-thread task runtime wiring the existing `ContextSwitch` HAL into
  the live scheduler; SP2 resumable EL0 tasks that timeshare a CPU; SP3
  the `spawn` syscall #12 + embedded-program registry; SP4 `init` launches
  the `session` process — overlapping P6e). Each `SP`-stage lands green
  over the whole project on its own. **SP0, SP1, SP2a, SP2b, and SP2c are
  landed, so SP2 is complete on aarch64:** the SP0 design note is
  `docs/src/architecture/multitasking.md`, and the `kernel/core::kthread`
  runtime (`spawn_kthread`, the `Yielder`, the per-task kernel stack) is
  host-tested and proven on `-M virt` by
  `tests/integration/kthread_switch_qemu_{aarch64,riscv64,x86_64}` — two
  kthreads ping-pong through the real `ContextSwitch::switch`, now a
  production scheduling path on every arch. (The x86_64 sibling was the
  first on-metal first-resume into a real Rust trampoline and surfaced a
  latent `TaskCtx::prepare` rdi-slot + stack-alignment bug, now fixed.)
  SP2a added the arch-neutral EL0-reschedule machinery, and **SP2b makes
  PID 1 reach EL0 as a resumable user kthread**: `KernelArch` exposes a
  `ContextSwitch` (`type Cs` + `context_switch()`), the aarch64 port gained
  `paging::activate_user_root` for the per-task `pre_resume` hook,
  `InitSpawnCtx::admit_init` admits PID 1 via `spawn_user_kthread`, and the
  `KernelDispatchHook` producer maps `yield`/`exit` to a `Reschedule`
  outcome (the handlers no longer drive the scheduler directly). The
  production `spawn_init_qemu_aarch64` vertical reaches EL0 through that
  full path. **SP2c then proves two EL0 user tasks timeshare one CPU**:
  the new `tests/integration/spawn_el0_timeshare_qemu_aarch64` vertical
  builds two hardware-isolated EL0 address spaces from the pure-Rust
  `rustos-test-el0-yielder` fixture (it links the new `rustos_rt::yield_now`
  wrapper), admits each as a resumable user kthread via `spawn_user_kthread`,
  and drains the cooperative `step` loop while a dispatch callback maps each
  task's `yield`/`exit` to `reschedule_current` — verified green on `-M
  virt`. **SP3 (the `spawn` syscall #12 + embedded-program registry) is
  staged SP3a/SP3b; SP3a is landed:** the `abi-v1` `spawn` syscall #12
  (`CAP_PROC_SPAWN`, audited) is wired end to end — `lib/abi` row + frozen
  tests, the `ros_sys_spawn` C stub + regenerated header, the
  `kernel/syscall` dispatch arm + recomputed `SYSCALL_TABLE_HASH` — plus
  the `kernel/core` path-keyed `ProgramRegistry` and the fail-closed
  `ProcessSpawn`/`SpawnCtx` seam (default `NULL_PROCESS_SPAWN` →
  `NotImplemented`, mirroring `NULL_CONSOLE`). The `spawn` handler
  copies-in the path, resolves it, and admits a **Ready** resumable user
  kthread through `SpawnCtx::admit_process` (host-proven by a `ProcessSpawn`
  double + 8 host tests). **SP3b and SP4 are now landed too, so P6d is
  complete:** the real aarch64 `ProcessSpawn` producer
  (`kernel/rustos-kernel/src/spawn_producer.rs`) builds each child a fresh,
  hardware-isolated identity address space (window mask-derived, as PID
  1's) whose page tables come from
  the kernel's live `FrameAllocator` through a boot-cached `kernel/mem`
  `FrameTableSource` (§24.1 — no fixed reserve, capacity scales with RAM;
  without switching the spawning caller's `TTBR0_EL1`), drives the audited
  `spawn_image` + `admit_process`, and is
  installed via `BootInfo::with_spawn`; the kernel `build.rs` now embeds both
  `init` and the `Shell` session program through one `elf2rxe` helper. PID 1
  `init` (granted `CAP_PROC_SPAWN`) spawns `config.session()`
  (`/Apps/Shell.app/Run`) through `rustos_rt::spawn` and keeps running; the
  `tests/integration/spawn_session_qemu_aarch64` vertical proves both
  processes run on `-M virt` (PASS on two `ProcessSpawned` + three audited
  syscalls — the session's gated banner+exit is necessarily last).
- **P6e — real shell REPL + session supervision `[x]`.** The `session`
  program `init` launches is currently a banner+exit `Run` stub in the
  `Shell` bundle; P6e wires the existing `rustos-shell` interpreter library
  into it (a real REPL) and has `init` supervise the session across its
  lifetime (restart, reap). **Design correction (binding, AGENTS.md §20):**
  the shell must do its text I/O over its **inherited standard streams
  (fd 0 `stdin` / fd 1 `stdout` / fd 2 `stderr` / fd 3 `stdinfo`)**, *not*
  over the kernel-discovered console via `console_read`/`console_write`.
  Binding the REPL to the discovered console hard-codes "whichever console
  the kernel found" into the shell — ambient authority (§4) and hidden
  device coupling (§17.3/§17.4). Reading fd 0 / writing fd 1 makes the same
  `rustos-shell` binary "just work" whether started on a UART, a
  framebuffer console, a network socket, or a WM terminal surface, with
  **zero** shell-side changes — only the *backing* of its descriptors
  differs. The gap this exposes: `abi-v1`'s startup vector
  (`lib/abi/src/process.rs`) carries args/env/canary but **no descriptor
  table** — there is no notion of inherited streams yet. So P6e is staged
  so that P6e-1/P6e-2 build the device *backing* and P6e-3a adds the
  stream layer the shell actually binds to:
  - **P6e-1 — `console_read` `abi-v1` syscall + kernel seam `[x]`.** The
    input counterpart of P6a's `console_write`, and — together with it —
    reframed as the bootstrap **device backing** the stream layer attaches
    to fd 0/1, *not* the shell's interface (AGENTS.md §20). Syscall **#13**
    (`SyscallNumber::CONSOLE_READ`) gated by the new
    `CapabilityId::CONSOLE_READ` (**id 19**), appended to the `lib/abi`
    source of truth (table row, regenerated C header, recomputed
    `SYSCALL_TABLE_HASH`). A `ConsoleRead` seam in `kernel/core::console`
    (default `NULL_CONSOLE_READ`, fail-closed `NotImplemented`, installed
    via `BootInfo::with_console_read`) and a `console_read` handler that
    reads into a bounded (`CONSOLE_READ_MAX`) kernel staging buffer and
    `copy_out`s to the caller (short/zero reads valid, defensive clamp,
    `BadAddress` on a faulting/unregistered caller). `lib/rt::console_read`
    + `lib/abi-sys::ros_sys_console_read` wrappers. Host-proven by 7
    `kernel/core` tests + the rt/abi-sys/abi drift+marshalling tests; the
    dispatcher reachability/fuzz/proptest doubles gained the new arm. **No
    device read is wired yet** — the aarch64 serial has no RX primitive, so
    `console_read` fails closed everywhere until P6e-2.
  - **P6e-2 — UART RX device + wiring `[x]`.** A non-blocking
    console-input read primitive was added to the aarch64 serial path:
    `ConsoleModel::rx_ready` decodes each model's receive-status bit
    (PL011 `UARTFR.RXFE` set = FIFO empty; mini-UART `AUX_MU_LSR_REG`
    bit 0 set = data ready), reusing the existing data/status offsets
    since the RX registers coincide with TX on both models;
    `serial::getchar`/`read_console_bytes` drain the RX FIFO into the
    caller's buffer and stop at the first absent byte — no busy-wait
    (§2.1), so an empty read is a valid zero-length short read. The
    zero-sized `UartConsole` now implements `ConsoleRead` (`Ok(0)` inert
    on host), and `boot_aarch64::enter_kernel_core` installs it through
    `BootInfo::with_console_read(&UART_CONSOLE)` beside the existing
    `.with_console`; kernel-core's init pipeline wraps whatever device the
    boot path installed in `BlockingConsoleRead`
    (`kernel/core/src/console.rs`), which parks an empty-handed
    `stream_read` caller on the scheduler (`reschedule_current`, the
    `wait`-syscall poll-and-park loop) and re-polls on redispatch — the
    backing owns blocking (§20), so user space never sees a spurious
    end-of-input. This completes the **bootstrap backing** — it feeds
    fd 0's backing object (P6e-3a), it is **not** called directly by the
    shell. The receive-bit decoders are host-unit-tested (2 new
    `console` tests + 1 `aarch64::arch_wrapper` adapter test); the
    freestanding aarch64 kernel builds clean.
  - **P6e-3a — standard-stream ABI + fd table `[x]`.** The two console
    syscalls were evolved **in place** (§2.13) into fd-keyed stream ops:
    `stream_write(fd, buf, len)` (#11) and `stream_read(fd, buf, len)`
    (#13), arg_count 3 with a leading `U32 fd`, appended-row-stable
    capabilities (`CAP_CONSOLE_WRITE`/`CAP_CONSOLE_READ` kept as the coarse
    "may use a console-backed stream" gate). `lib/abi/src/process.rs` gained
    the per-process descriptor model — `STDIN`/`STDOUT`/`STDERR`/`STDINFO`,
    `STD_STREAM_COUNT`, `StreamMode{Closed,Read,Write}`, and `DescriptorTable`
    (`closed()`/`standard()`/`mode()`) — established at spawn and held per
    task in `AddressSpaceRegistry` (new `set_streams`/`streams`, cleared on
    `withdraw`). The `stream_write`/`stream_read` handlers resolve `fd`
    against the caller's table **before** any state and fail closed with
    `NotFound` unless the direction matches; both production admit paths
    (`admit_init`, `admit_process`) install `DescriptorTable::standard()`, so
    a process's fd 0 reads / fd 1/2/3 write the discovered console the boot
    path installed (the P6e-1/P6e-2 UART backing **reused behind the
    stream**, not exposed). `lib/rt` now exposes `stdout`/`stderr`/`stdinfo`/
    `stdin` (over fd 0/1/2/3, `console_*` removed); `lib/abi-sys` exports
    `ros_sys_stream_write`/`ros_sys_stream_read`; `init` + the `Shell`
    session write their banner via `rustos_rt::stdout`. C header regenerated
    (`ROS_SYS_STREAM_WRITE`/`_READ`) and `SYSCALL_TABLE_HASH` recomputed
    (`1cfbad…`); the abi-check + c-header drift guards are green. Proven
    host-side (the `lib/abi` descriptor-table tests, the `aspace` stream-map
    tests, the 11 reworked + 3 new `kernel/core` handler gate tests, the
    rt/abi-sys fd-marshalling tests) and on `-M virt`: the
    `spawn_session_qemu_aarch64` vertical now proves a spawned child writes
    fd 1 over the discovered-UART backing (the shell banner lands via
    `stdout`). Whole-project gate (fmt / `cargo xtask ci` / `fuzz --secs 5`
    / `soak.sh both` / `cargo xtask test --qemu`) **green on this host**.
    Real UART **RX** over fd 0 on silicon remains an on-metal item (no
    deterministic `-M virt` serial-RX injection, consistent with P6e-2).
  - **P6e-3b — shell REPL over its streams + `init` supervision `[x]`.**
    Wire `rustos-shell` to read fd 0 / write fd 1 (and emit `stdinfo` on
    fd 3 per §20) through the `lib/rt` standard-stream wrappers, with
    `init` supervising the session (restart, reap). The shell contains
    **no** reference to `console_*` or to any device. Staged into the REPL
    itself (P6e-3b-i) and `init` supervision (P6e-3b-ii) — **both landed**.
    - **P6e-3b-i — shell REPL over fd 0/1/2/3 `[x]`.** The `Shell` bundle's
      `Run` binary (`userland/shell/shell/src/run.rs`) no longer prints a
      banner and exits: it runs the sibling `rustos-shell` interpreter as a
      read-eval-print loop (the new `repl` lib module) over its **inherited
      standard streams** (`AGENTS.md` §20). `repl::run` reads command lines
      from fd 0 (`rustos_rt::stdin`, reassembling lines across reads,
      stripping CRLF, capping a line at 4 KiB and discarding an over-length
      line), runs each through `Shell::run_line`, writes the prompt + output
      to fd 1/2 through the `RtConsole` seam, and emits one `omission`
      `StdInfoRecord` on fd 3 when a line is dropped (§20.1). It binds to fd
      0/1/2/3 only — **no** `console_*` or device reference (ambient authority
      §4 / hidden coupling §17.3/§17.4). A zero-length read is end of input
      (clean exit); *blocking* is the stream backing's job (§20), and the
      kernel-core `BlockingConsoleRead` backing provides it (P6e-2), so an
      interactive session sits at its prompt until input arrives. The
      `RtProcessHost` launches a single
      bare-path command via `spawn` + reaps via `wait`, failing closed
      (`NotImplemented`) on pipes/redirs/args/signals/`cd` the current `spawn`
      ABI cannot express. `lib/abi` gained a tested `Errno::from_i32` decoder
      (single source of truth, §2.2; no C-header/hash impact — a method, not
      an ABI type change) and `rustos_rt::stdin` now clamps a negative
      `-errno` to a zero-length read and clamps the count to `buf.len()`
      (defence in depth, §5.4). Host-proven (6 new `repl` tests over scripted
      stdin/stdinfo + `Console`/`ProcessHost` fixtures; 3 new `lib/rt` stdin
      tests; the `Errno::from_i32` round-trip test) and freestanding-built on
      all three bare-metal targets; the `spawn_session_qemu_aarch64` vertical
      proves the interactive loop (the session blocks at its prompt and the
      runner types a scripted `exit\n` at the guest's serial input). Docs:
      `docs/src/userland/shell.md`.
    - **P6e-3b-ii — `init` session supervision `[x]`.** PID 1 `init` no
      longer spawns-and-forgets the session: `userland/system/init/src/run.rs`
      now runs a fail-closed **supervise loop** — `spawn` the session, `wait`
      on exactly that child (blocking until it exits, reaping it), then
      relaunch it. The loop is bounded by a small `SESSION_SPAWN_BUDGET`
      crash-loop guard: a session that blocks on input runs for PID 1's whole
      life and never approaches it, but one that exits instantly (no input
      backing) stops the loop at `EXIT_SESSION_EXHAUSTED` rather than
      busy-spinning on `spawn` (`AGENTS.md` §2.1), and a failed `spawn`/`wait`
      is fail-loud (`EXIT_SESSION_FAILED`/`EXIT_WAIT_FAILED`, §2.9). The
      userland + kernel-bookkeeping pieces were already wired — the production
      aarch64 pipeline wires the `KernelProcessWait` producer
      (`kernel_core::run_phases`), the `spawn` admit path's `register_child`,
      and the `exit` handler's `record_exit`, and `admit_init`'s drive loop
      re-dispatches the parked `init` after the session exits — but the
      supervise loop exposed a latent **aarch64 arch defect** that hung the
      vertical (see the errata below). This changes `init`'s audited-syscall
      sequence, so the `-M virt` vertical assertions were reworked:
      `spawn_session_qemu_aarch64` now keys PASS on **three** `ProcessSpawned`
      (init + two session launches — the second launch proves the first was
      reaped and relaunched) + **four** audited syscalls (`init`'s `spawn`,
      `init`'s `wait`, the session's `exit`, `init`'s second `spawn`); the
      sibling `spawn_init_qemu_aarch64` still PASSes (its witness is now
      `init`'s first audited syscall, the `spawn`, instead of an `exit`) and
      its doc was updated. The session now **blocks** on fd 0 (the kernel-core
      `BlockingConsoleRead` backing) instead of exiting at end-of-input, and
      the runner gained deterministic `-M virt` serial-RX injection
      (`SerialInjection`: pipe QEMU stdin, type a scripted line once the
      guest prints its prompt marker), so the vertical exercises the full
      interactive cycle: prompt → injected `exit\n` → reap → relaunch → the
      second session blocks at its prompt. The session's capability set is
      `{CAP_CONSOLE_WRITE, CAP_CONSOLE_READ}` on every port (§2.2); ports
      with no console-read backing (x86_64, riscv64) keep failing closed at
      `NULL_CONSOLE_READ`, so their session verticals still witness the
      EOF-exit supervision path. Docs: `docs/src/userland/init.md` ("Session
      supervision").
    - **Errata — aarch64 exception return-state save/restore `[x]`.** The
      P6e-3b-ii supervise loop hung the `spawn_session_qemu_aarch64` vertical
      (`Outcome::Timeout`): `init`'s `wait` reaped the session and returned,
      but `init` never reached its relaunch `spawn`. Root cause was a latent
      defect in the aarch64 EL1 exception trampoline (`kernel/arch/aarch64/
      src/vectors.s`): it saved only `x0..x30`, **not** `ELR_EL1`/`SPSR_EL1`/
      `SP_EL0`, relying on the live system registers across `eret`. That holds
      only when a handler returns directly — but a parked `wait`/`yield`
      (SP2) suspends the task **mid-handler** and switches to another task,
      whose own trap/`eret` clobbers those registers, so the resuming
      exception `eret`ed `init` to the session's PC/stack. (The SP2c
      `spawn_el0_timeshare` vertical masked it: two *identical* programs at
      identical VAs resume "correctly" at the wrong-but-equal PC.) Fix: the
      common trampoline now saves `ELR_EL1`/`SPSR_EL1`/`SP_EL0` into an
      enlarged 288-byte per-exception frame (GP-register offsets unchanged —
      the `[u64; SAVED_GPRS]` syscall view is intact) and writes them back
      before `eret`, making every exception's resume self-contained across a
      cooperative context switch. `spawn_session_qemu_aarch64` now PASSes
      (3 `ProcessSpawned` + 4 audited syscalls). The riscv64/x86_64 trap
      vectors carry the same latent pattern but have no EL0 spawn/wait
      timeshare path wired yet, so it is unreachable there today and is
      folded into those ports' user-mode bring-up follow-ons. Docs:
      `docs/src/platform/aarch64.md` (Interrupts).
    - **Prerequisite — `lib/rt` `mem_map`-backed `#[global_allocator]`
      `[x]`.** The `rustos-shell` interpreter is `no_std + alloc`, but the
      freestanding userland runtime had no heap, so the shell could not link
      it. `lib/rt` now registers a `#[global_allocator]`
      (`lib/rt/src/heap.rs`): a free-span allocator over a fixed-base virtual
      arena that grows by `mem_map(FIXED)` and shrinks by `mem_unmap`,
      first-fit with alignment-padding return + neighbour coalescing, real
      free, deterministic-OOM-to-null (`AGENTS.md` §4/§2.9), no re-zero on
      free (the kernel already zeroes on map/free, §2.16). The pure free-span
      bookkeeping is host-unit-tested over a fake pager; the aarch64 `-M virt`
      vertical `tests/integration/heap_qemu_aarch64` proves it end to end — a
      pure-Rust EL0 fixture (`tests/integration/heap_program`) Box-allocates,
      grows a `Vec` across pages, reallocates after freeing, verifies every
      value, and exits 0 (PASS), with the allocator-issued `mem_map`/
      `mem_unmap` `svc`s routed through the live `MemMap` producer.
      **Verified green under QEMU on `-M virt`.** Design note:
      `docs/src/architecture/memory.md` §7d. This unblocks the REPL; the REPL
      itself + `init` supervision (which also needs a process-wait syscall)
      remain.
    - **Prerequisite — `wait` process-wait syscall (SP6) `[x]`.** Both the
      shell's foreground job control and `init` supervising the session
      (reap, restart) need a way to block on and reap a child — `spawn` was
      spawn-and-forget. **SP6 is COMPLETE** (`plans/SPAWN.md` SP6): SP6a
      landed the `abi-v1` surface (`SyscallNumber::WAIT` #16 + `WAIT_ANY`, the
      `wait(I32 pid, UserPtr status) -> U64` row, unprivileged + audited),
      the `ros_sys_wait` C stub + regenerated header, the `rustos_rt::wait`
      wrapper, the `kernel/syscall` dispatch arm + doubles, and the
      fail-closed `kernel/core::procwait::ProcessWait` seam + handler. **SP6b
      (this session)** landed the scheduler-side producer: the `ProcessWait`
      trait gained default-no-op `register_child`/`record_exit` hooks (so the
      null default + test doubles stay inert and no `new()` churn), the real
      `KernelProcessWait<A>` owns a `SpinLock<ProcessTable>` and blocks a
      waiting parent by cooperatively parking it via `reschedule_current(…,
      Yield)` until a child is reapable (fail-closed `NotImplemented` if no
      user kthread is published — never a busy-spin), `exit` records the code,
      the `spawn` admit path registers the parent→child link, and `run_phases`
      installs the producer via the hook's new `with_process_wait`. The
      aarch64 `-M virt` vertical `tests/integration/wait_qemu_aarch64` (+ the
      two-role `tests/integration/wait_program` fixture) proves a parent reaps
      a child that exited with a known code and reads it back, exiting 0 —
      **verified green under QEMU on `-M virt`**. This unblocks the REPL +
      `init` supervision.

**Done when:** under `-M raspi4b`, the kernel reaches `init` in EL0 and
`init` emits its first line on the console (framebuffer if present, else
the discovered UART), then starts the user's shell; a vertical asserts the
EL0 transition + the `init` banner. (This is the "boot into user mode"
milestone.)

**Landed (proven on `-M virt`).** All of P6a–P6e are `[x]`: the production
aarch64 kernel reaches EL0, PID 1 `init` writes its banner over its
inherited `stdout`, launches the `Shell` session through the `spawn`
syscall, and now **supervises** it (`spawn`→`wait`/reap→relaunch). The EL0
transition + banner are proven by `spawn_init_qemu_aarch64` and the
supervision by `spawn_session_qemu_aarch64`, both on `-M virt` — QEMU 8.2.2
has no `raspi4b` and `raspi*` performs no DTB hand-off (the standing P2
gap), so the `-M raspi4b` form of this gate is an on-metal acceptance item.

**P6 follow-on — SP5 `mem_map`/`mem_unmap` (runtime anonymous memory).**
A spawned process otherwise has only its fixed spawn-time image, so it
cannot obtain a heap; SP5 (`plans/SPAWN.md`) adds the `mmap`-style
anonymous map/unmap pair a future `lib/rt` `malloc`/`free` layers over.
**SP5-0 (design note) and SP5a (the `abi-v1` surface + fail-closed seam)
are landed:** `SyscallNumber::MEM_MAP` (#14) / `MEM_UNMAP` (#15), the
`MapFlags` type (with `FIXED`), the appended `Errno::OutOfMemory` (#20),
the `ros_sys_mem_map`/`ros_sys_mem_unmap` C stubs + regenerated header,
the dispatcher arms, and `kernel/core`'s `MemMap` seam (`NULL_MEM_MAP` /
`with_mem_map`, unprivileged + unaudited, fail-closed `NotImplemented`).
**SP5b-1 is also landed:** the reusable, host-proven `kernel/mem::anon`
live-address-space producer (`map_anonymous`/`unmap_anonymous` — zero on
map/free, W^X `RW|USER`, deterministic OOM, fail-closed all-or-nothing
reclaim, per-page TLB flush). **SP5b-2 is also landed (SP5 complete):** the
aarch64 `-M virt` EL0 vertical `tests/integration/mem_map_qemu_aarch64`
wires the producer through the `kernel/core` `MemMap` seam — it builds one
isolated EL0 space with `spawn_image`, **retains** it live behind a `MemMap`
producer over `map_anonymous`/`unmap_anonymous`, admits the program as a
resumable user kthread, and routes the program's `mem_map`/`mem_unmap`
`svc`s through it; the pure-Rust EL0 fixture
`tests/integration/mem_map_program` (linking the new
`rustos_rt::mem_map`/`mem_unmap` wrappers) maps a region (FIXED),
writes+verifies a pattern, unmaps it, then faults on use — the fault handler
reports PASS, **verified green under QEMU on `-M virt`**. The **riscv64
sibling `tests/integration/mem_map_qemu_riscv64` is now landed too**: it
reuses the same pure-Rust `mem_map_program` fixture and the same
`kernel/mem::anon` producer over an Sv39 U-mode space, but drops into the
program through `spawn_image` + a direct `EnterUser::enter_user` (a single
task that only direct-returns from its `ecall`s, so the riscv64
cooperative-switch trap-save path stays off the critical path) and reports
the use-after-unmap page fault as PASS on `-M virt` (ids 4284–4287, **verified
green on this host**). The x86_64 sibling + production per-task live-space
retention still follow.

**P6 follow-on — kthread kernel-stack guard page.** The deep-`wait`-handler
overrun that silently corrupted the next task's snapshot (P6e-3b-ii) is now
defended: `kernel/core::kthread::BoxStack` carries a poison-filled guard
page immediately *below* the usable stack and `dispatch_step` verifies its
canary on every switch-back, failing the task closed on an overrun rather
than letting it reach the heap neighbour (`AGENTS.md` §4 / §2.9 / §2.17,
host-proven; the same emulation `kernel/mem`'s slab guard documents). **This
is the real, non-deferred defence — not the old 64 KiB limit bump.** The
*deployment* form, which turns the overrun into an immediate hardware fault
instead of a next-reschedule detection, is now **landed `[x]`** (G1–G3c):
- Lay the guard region on its own 4 KiB page (the stack allocation is
  already page-multiple) and **unmap** that page in the kernel's own
  (`TTBR0`) tables, so a write into it faults synchronously.
- This needs a kernel-self-mapping path: the aarch64 port currently
  identity-maps RAM as 1 GiB **blocks**, so unmapping one 4 KiB page means
  splitting the block → 2 MiB → 4 KiB and a local TLB invalidation. Build
  it behind the Arch HAL (§17.2, no `cfg(target_arch)` leak) on the Stage 6
  page-table primitives, with the per-arch conformance vertical, and route
  `BoxStack` through it. Until it lands the poison-canary emulation above
  is the binding defence (§2.17 — a guard now, the fault-form staged, never
  "security later").

  **The fault-form is now complete on aarch64 (G1–G3c all `[x]`).** It was
  built *properly* (the running kernel cannot break-before-make-shatter the
  1 GiB block it is itself executing on and stacked in: the BBM "break"
  window would momentarily unmap the running stack/code), so it was staged
  G1..G3:

  - **G1 — aarch64 page-table block-split primitive `[x]`.** The missing
    foundation: `rustos_arch_aarch64::paging::AddressSpace::split_block(vaddr)`
    re-expresses the coarse block covering `vaddr` at 4 KiB granularity (L1
    1 GiB block → table of 512 × 2 MiB blocks, then the covering 2 MiB block
    → table of 512 × 4 KiB pages), preserving the output address and **every**
    attribute bit (`shatter_block_into` copies `desc & !ADDR_MASK`, setting
    `TABLE_OR_PAGE` only at L3). It only *adds* table levels that reproduce
    the existing translation — never invalidating a live address — so it is
    safe against the running region (no break-before-make), is idempotent, and
    fails closed (`Misaligned`/`NotMapped`/`PoolExhausted`). With it a single
    4 KiB page inside a former block can be torn down with the existing
    `MmuAddressSpace::unmap` + `TlbShootdown::flush_page`. Host-proven (4 new
    `paging_tests.rs` tests: identity-preserving shatter, post-split
    single-page unmap, Device-attr preservation, idempotency + fail-closed)
    and end-to-end on `-M virt` by `tests/integration/stack_guard_qemu_aarch64`
    (ids 4300–4302): build an identity space, `split_block` a RAM block,
    enable the MMU, write+read-back a sentinel through the guard page (the
    split preserved the mapping live), then `unmap` + `flush_page` that one
    page and read it → synchronous data abort → PASS. **Verified green under
    QEMU on `-M virt` on this host.** Doc: `docs/src/platform/aarch64.md`
    ("Splitting a block: the guard-page fault-form").
  - **G2 — guarded kthread-stack arena (boot mapping) `[x]`.** Gives kthread
    kernel-stacks a region whose guard pages can be unmapped without ever
    shattering the block the CPU runs on, by mapping a kthread-stack arena
    at 2 MiB/4 KiB granularity at boot (so a guard page is its own L3 leaf
    reachable through the G1 split run on a 2 MiB block that holds no running
    context). `rustos_arch_aarch64::paging::AddressSpace::prepare_guard_arena(base, len)`
    is `split_block` applied to every 2 MiB block the arena spans — idempotent,
    fail-closed (`Misaligned`/`NotMapped`/`PoolExhausted`), BBM-free against the
    live regime (it only *adds* table levels reproducing the translation), and
    needs no TLB maintenance (3 host tests in `paging_tests.rs`). `mem_map.rs`
    carves a 2 MiB-aligned, 2 MiB guard arena out of the usable RAM window
    (above the kernel image) and marks it `Reserved` so the allocator never
    hands its frames out, returning a `MemoryLayout { map, arena }` (rewritten
    + 2 new host tests proving the regions tile the window with no gap/overlap;
    a too-small window degrades to no arena, fail closed). `boot_aarch64` now
    keeps the live boot `AddressSpace` (`enable_mmu_and_vectors` returns it),
    fine-maps the arena over the *active* tables after discovery, and logs a
    `guard_arena_prepared` audit field. The per-arch conformance vertical
    `tests/integration/stack_arena_qemu_aarch64` (ids 4300-range 4303–4305)
    builds an identity space, prepares a 2 MiB-aligned arena that is its own L2
    block, enables the MMU, write+read-back-verifies a guard page, `unmap`s it
    + `flush_page`s it through the Arch HAL, proves the running stack (a
    *different* 2 MiB block) and a neighbouring arena page still work, then
    reads the unmapped page → synchronous data abort → PASS. **Verified green
    under QEMU on `-M virt` on this host.** Doc:
    `docs/src/platform/aarch64.md` ("A guard-page arena: the boot mapping").
    The boot-map change stays in `boot_aarch64` (the §17.2 arch-gated binary,
    no `cfg(target_arch)` leak); promoting `prepare_guard_arena` onto the Arch
    HAL `AddressSpace` surface is **G3**.
  - **G3a — `split_block` HAL promotion `[x]`.** The coarse-block split is
    now part of the architecture-neutral Arch HAL `AddressSpace` surface
    (`rustos_arch_api::mmu`, §17.2), so the kernel reaches it through one
    vocabulary instead of naming a concrete port. Two members:
    `block_split_support() -> BlockSplit` (each port's honest declaration,
    modelled on the §19.1/§19.10 `Mitigation`/`Tagging` profiles —
    `Supported` / justified `Unsupported` / tracked `Pending`, with the
    non-empty-justification rule the `mmu::conformance` vertical enforces)
    and a default-fail-closed `split_block(vaddr)` (returns the new
    `MapError::Unsupported` so a non-supporting port never silently
    no-ops, §2.9). aarch64 reports `Supported` and the HAL `split_block`
    forwards to its tested inherent body (one implementation, §2.2);
    riscv64 now reports `Supported` too (its Sv39 split landed — see
    "riscv64 Sv39 block split" below); x86_64 now reports `Supported` too
    (its four-level huge-page split landed — see "x86_64 four-level
    huge-page split" below); the `kernel/mem` `HostPageTable` double +
    `from_map_error`
    carry the new cases (`PageTableError::Unsupported`). Host-proven: the
    `mmu` conformance suite gained a block-split honesty check (declaration
    justified; non-`Supported` ports fail `split_block` closed — 4 new
    arch-api tests), aarch64 `paging_tests` proves the HAL method reaches
    the inherent body over `dyn AddressSpace`. Doc:
    `docs/src/platform/aarch64.md`
    ("Promoting the split onto the Arch HAL (G3a)"). **No QEMU vertical
    needed** — G1/G2 already prove the live aarch64 mechanism, now reached
    through the promoted surface.
  - **G3b-1 — `prepare_guard_arena` HAL promotion `[x]`.** The arena form
    of the split (G2 — `split_block` applied to every coarse block an arena
    spans) is now part of the architecture-neutral Arch HAL `AddressSpace`
    surface (`rustos_arch_api::mmu`, §17.2), beside the G3a `split_block`
    members. `AddressSpace::prepare_guard_arena(base, len)` defaults to a
    fail-closed `MapError::Unsupported`, so a port whose
    `block_split_support` is not `Supported` falls back to the software
    canary guard rather than silently pretending the arena was hardened
    (`AGENTS.md` §2.9 / §2.17); aarch64 (`Supported`) overrides it to
    forward to its inherent, fully-tested body (one implementation, §2.2).
    The `mmu::conformance` honesty check now also requires a non-`Supported`
    port to fail `prepare_guard_arena` closed; aarch64 `paging_tests` proves
    the HAL method reaches the inherent body over `dyn AddressSpace`.
    Host-proven; **no QEMU vertical needed** — G2 already proves the
    live aarch64 arena, now reached through the promoted surface. Doc:
    `docs/src/platform/aarch64.md` ("Promoting the arena onto the Arch HAL
    (G3b)").
  - **G3b-2 — `BoxStack` rewire over the G2 arena.** Route the kthread
    kernel-stack guard page through unmap-on-create over the G2 arena
    (canary fallback where a port's `block_split_support` is not
    `Supported`). The kthread kernel stack must be mapped in every space the
    task runs under, and each EL0 task runs the kernel on its *own* `TTBR0`,
    so the guard page is unmapped **per-task, in that task's own root** — by
    the arch spawn seam that builds the root, not generically in
    `kernel/core` (whose `UserAddressSpace` view is read-only by design,
    §2.4). Staged by spawn path:
    - **G3b-2-i — PID 1 (`init`) path `[x]`.** A grow-and-shrink block
      allocator `stack_arena::KTHREAD_STACK_ARENA` (`rustos-kernel`) hands
      kthread kernel stacks out of the boot-reserved arena (`boot_aarch64`
      `install`s it from the carved `(base, len)`); each `ArenaStack` is a
      one-page guard below the usable `KTHREAD_STACK_BYTES` stack, identical
      in geometry to `BoxStack`. The arena chains fresh 2 MiB blocks from the
      live `FrameAllocator` on exhaustion and returns idle chained blocks on
      `ArenaStack` drop (grow *and* shrink, §24.1; the boot block is never
      returned). `init_spawn`
      allocates one region and, on `init`'s *own* concrete `arch` space
      **before** it is switched to, `split_block(guard)` + `unmap(guard)`
      (no live access disturbed, no TLB maintenance), then hands the boxed
      stack to `kernel/core` through the new `InitSpawnCtx::admit_init`
      `stack: Box<dyn KernelStack + Send>` param (admitted via
      `spawn_user_kthread_with_stack`). No arena / failed split → fall back
      to a software-canary `BoxStack` (fail closed, §2.9/§2.17).
      `ArenaStack::check_guard` is the default `Ok(())` — the hardware fault
      is the defence, no canary to scan. Host-proven (`stack_arena` bump
      tests + `kthread`/`spawn`/`init` build); the `spawn_init` /
      `spawn_session` / `wait` `-M virt` verticals prove `init` still
      reaches EL0 and supervises the session on the arena stack. Whole gate
      (fmt / `cargo xtask ci` incl. `test --qemu` / `fuzz --secs 5` /
      `soak.sh both`) green on this host. Doc:
      `docs/src/platform/aarch64.md` ("Routing the kthread stack through the
      arena (G3b-2)").
    - **G3b-2-ii — runtime `spawn`-syscall path `[x]`.** The session
      (and anything it launches) now runs on an arena-backed,
      hardware-guarded kernel stack too — the `spawn_producer`/
      `admit_process` mirror of G3b-2-i. `kernel/core`'s
      `SpawnCtx::admit_process` grew the same `stack: Box<dyn KernelStack +
      Send>` parameter `InitSpawnCtx::admit_init` carries, routing the child
      through `spawn_user_kthread_with_stack` (`HandlerSpawnCtx` +
      the `RecordingSpawn` host double updated). `spawn_producer` allocates
      an `ArenaStack` from `KTHREAD_STACK_ARENA`, then `split_block(guard)`
      + `unmap(guard)` on the child's *own* `arch` root — which it builds
      but **never switches to** (the spawning caller keeps its own
      `TTBR0_EL1`), so the split/unmap only touch the child's tables through
      the caller's identity window, disturb no live access, and need no TLB
      maintenance — with the software-canary `BoxStack` fallback where no
      arena region is available or the split/unmap fails (fail closed,
      `AGENTS.md` §2.9/§2.17). Host-proven (the 11 `spawn` admit tests + the
      `stack_arena` bump tests; the aarch64 kernel builds clean); the
      `spawn_session` / `spawn_init` / `wait` `-M virt` verticals prove the
      session still runs and is supervised, now on the arena stack. Doc:
      `docs/src/platform/aarch64.md` ("Routing the kthread stack through the
      arena (G3b-2)").
    - **G3b-2-iii — x86_64 PID 1 + runtime `spawn` paths `[x]`.** The
      x86_64 cross-port sibling of G3b-2-i/-ii: both production seams
      (`x86_64::init_spawn`, `x86_64::spawn_producer`) now run on
      arena-backed, hardware-guarded kernel stacks instead of the
      software-canary `BoxStack`. The boot path carves the arena out of the
      *firmware* memory map — `mem_map::carve_guard_arena_from_map(map,
      ram_bytes, max_addr)` scans the multi-region Multiboot2 map for the
      first `Usable` region that can host a whole 2 MiB-aligned, §24.2
      policy-sized block below the 4 GiB identity window, `reserve_range`s
      it, and `boot::try_boot` `install`s it into `KTHREAD_STACK_ARENA`
      (audited, `EventId(4097)`; no usable region ⇒ no arena ⇒ software
      canary, fail closed §2.9). Each seam then allocates an `ArenaStack`
      (chained-grow bounded to the identity window) and `split_block(guard)`
      + `unmap(guard)` on the task's *own* PML4 — `init` before its
      `arch.switch()`, the `spawn` producer on the inactive child root — so
      an overrun faults synchronously under the task's own CR3 (§4 / §2.17),
      with the `BoxStack` fallback where no arena region is available or the
      split/unmap fails. `publish_reclaim_frames` returns idle chained
      blocks to the live allocator on `ArenaStack` drop (§24.1). The
      `mem_map`/`stack_arena` infra and the production glue are shared with
      aarch64, gated to the bare-metal `kernel_isa` ports (one body, §2.2);
      the firmware carve is host-tested. The
      `spawn_init`/`spawn_session`/`spawn_program`/`wait`/`stack_overrun`
      `-M virt` x86_64 verticals prove `init` still spawns + supervises the
      session and an overrun still faults, now on the arena stack. **No ABI
      change.** Doc: `docs/src/platform/x86_64.md` ("Routing the kthread
      stack through the arena (G3b-2)").
    - **G3b-2-iv — riscv64 PID 1 + runtime `spawn` paths `[x]`.** The
      riscv64 cross-port sibling of G3b-2-iii: both production seams
      (`riscv64::init_spawn`, `riscv64::spawn_producer`) now run on
      arena-backed, hardware-guarded kernel stacks instead of the
      software-canary `BoxStack`. `boot_riscv64::try_boot` carves the arena
      out of its FDT-derived two-region map with the same shared
      `mem_map::carve_guard_arena_from_map` (§24.2 policy sized from the
      summed `Usable` bytes, bounded to the seams' 4 GiB Sv39 identity
      window), `install`s it into `KTHREAD_STACK_ARENA`, and audits the
      decision through the shared `mem_map::log_guard_arena` body
      (`EventId(4098)`; the former per-port copy in `boot` was folded into
      it, §2.2). Each seam allocates an `ArenaStack` (chained-grow bounded
      to the identity window; `publish_reclaim_frames` returns idle blocks
      on drop, §24.1) and `split_block(guard)` + `unmap(guard)` on the
      task's *own* Sv39 root — `init` before its `arch.switch()`, the
      `spawn` producer on the never-activated child root — so an overrun
      faults synchronously under the task's own `satp` (§4 / §2.17), with
      the `BoxStack` fallback where no arena region is available or the
      split/unmap fails (§2.9). The `mem_map`/`stack_arena` infra is the
      same shared body, its gates widened to `kernel_isa = "riscv64"`; the
      carve gained a riscv64 `virt`-shaped host regression case (8 cases
      total). The `spawn_init`/`spawn_program`/`spawn_session`/`wait`/
      `stack_overrun` `-M virt` riscv64 verticals prove `init` still spawns
      + supervises the session and an overrun still faults, now on the
      arena stack. **No ABI change.** Doc: `docs/src/platform/riscv64.md`
      (PID 1 seam / spawn producer bullets + the G1/G2 deployment note).
  - **G3c — production fault-form on `-M virt` `[x]`.** Proves an
    overrunning kthread takes a synchronous data abort, not a
    next-reschedule canary detection. `tests/integration/stack_overrun_qemu_aarch64`
    builds a stage-1 identity `AddressSpace`, prepares a 2 MiB-aligned guard
    arena (`prepare_guard_arena`, G2), carves one kthread stack region
    `[guard page | usable stack]` out of it, installs the EL1 vectors + a
    `fault` handler, enables the MMU, then `unmap`s the guard page through
    the Arch HAL + `flush_page`s it — exactly the G3b-2 production mechanism.
    It then builds the live `rustos-kernel-sched-eevdf` `Scheduler` over
    `Aarch64Arch`, admits a kthread on that stack via
    `kernel_core::spawn_kthread_with_stack` (the production runtime path),
    and drives the cooperative `step` loop. The kthread body overruns its
    stack by touching the highest byte of the guard region (the first byte a
    contiguous downward overrun crosses); because that page is unmapped the
    access raises a synchronous data abort *while the kthread runs* (the
    abort is taken on the still-healthy usable stack above the guard, so the
    EL1 trampoline does not nest-fault), the handler confirms the cause
    (`ESR_EL1` abort) + faulting address (`FAR_EL1` in the guard page), and
    reports PASS via semihosting. A regression that left the page mapped lets
    the body return cleanly; the drain loop then reports FAILURE explicitly
    rather than passing (§2.9). Enrolled in
    `tools/xtask/src/commands/qemu_tests.rs` (single CPU, 60 s); **verified
    green under QEMU on `-M virt` on this host**. With G3c landed the
    guard-page fault-form (G1–G3) is complete on aarch64. Doc:
    `docs/src/platform/aarch64.md` ("Proving the overrun fault-form (G3c)").
  - **riscv64 Sv39 block split + guard-page fault-form (G1/G2) `[x]`.** The
    cross-port follow-on bringing the guard-page fault-form to riscv64. The
    `kernel/arch/riscv64::paging::AddressSpace` now declares
    `block_split_support() == BlockSplit::Supported` and implements the
    inherent `split_block(vaddr)` (a level-2 1 GiB gigapage leaf → table of
    512 × 2 MiB megapage leaves, then the covering level-1 megapage leaf →
    table of 512 × 4 KiB page leaves) + `prepare_guard_arena(base, len)`
    (`split_block` over every 2 MiB block the arena spans), with the HAL-trait
    overrides forwarding to them (one body, §2.2). Sv39 carries the same
    R/W/X/U/A/D leaf encoding at every level, so the shared `shatter_pte_into`
    helper changes only the PPN per sub-entry (preserving every permission
    bit); the split only *adds* table levels reproducing the existing
    translation, so it is break-before-make-free, idempotent, and fails closed
    (`Misaligned`/`NotMapped`/`PoolExhausted`). Host-proven (the `paging_tests`
    split/identity/idempotency/fail-closed/arena/HAL-forward suite replaced the
    old `Pending` fail-closed test) and end to end on `-M virt` by
    `tests/integration/stack_guard_qemu_riscv64` (the sibling of
    `stack_guard_qemu_aarch64`, enrolled in
    `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s): split a coarse
    leaf covering a `GUARD_PAGE` static, turn paging on, sentinel
    write/read-back, then `unmap` + `flush_page` the page and read it → load
    page fault (`scause` 13, `stval` == guard page) → PASS. **Verified green
    under QEMU on `-M virt` on this host.** The production *runtime*
    fault-form is now proven on riscv64 too (G3c below). Doc:
    `docs/src/platform/riscv64.md` ("Sv39 block split + guard-page
    fault-form (G1/G2)").
  - **riscv64 production guard-page fault-form (G3c) `[x]`.** The cross-port
    sibling of `stack_overrun_qemu_{aarch64,x86_64}`: proves an *overrunning
    kthread* on riscv64 takes a **synchronous store page fault while running**
    under the live scheduler — the production runtime payoff, not the deferred
    next-reschedule poison-canary detection a heap-backed `BoxStack` falls back
    to. The new `tests/integration/stack_overrun_qemu_riscv64` (enrolled in
    `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s) builds an Sv39
    4 GiB-identity space, re-expresses a 2 MiB-aligned guard arena at 4 KiB
    granularity (`prepare_guard_arena`, G2), installs the S-mode trap vector +
    a `fault` handler, turns paging on, then `unmap`s + `flush_page`s one
    kthread stack's one-page guard through the Arch HAL (the production
    mechanism), builds the live `rustos-kernel-sched-eevdf` `Scheduler` over
    `RiscvArch`, and admits a kthread on that arena stack via
    `spawn_kthread_with_stack` (the **production runtime path**) laid out
    `[guard | usable]`. The kthread overruns into the unmapped guard page → a
    synchronous store page fault (`scause` 15, `stval` in the guard page), the
    `fault` observer confirms the cause + faulting address → PASS; a body that
    returns without faulting drains the `step` loop and fails loudly (§2.9).
    **Verified green under QEMU on `-M virt` on this host.** This proves the
    *mechanism*; the production `riscv64::init_spawn` /
    `riscv64::spawn_producer` kernel stacks now run on the boot-reserved
    arena (G3b-2-iv). **No ABI change.** Doc:
    `docs/src/platform/riscv64.md` ("Proving the overrun fault-form
    (G3c)").
  - **x86_64 four-level huge-page split + guard-page fault-form (G1/G2)
    `[x]`.** The last `BlockSplit::Pending` port brought to `Supported`, so
    all three bare-metal ports now declare and implement the split.
    `rustos_arch_x86_64::paging::AddressSpace` implements the inherent
    `split_block(vaddr)` (1 GiB PDPTE huge leaf → PD of 512 × 2 MiB huge
    leaves, keeping the page-size bit; then the 2 MiB PD huge leaf → PT of
    512 × 4 KiB pages, **clearing** the page-size bit since at PT level bit 7
    is PAT — Intel SDM Vol 3A §4.5) + `prepare_guard_arena(base, len)`
    (`split_block` over every 2 MiB block the arena spans), with the
    HAL-trait overrides forwarding to the inherent bodies (one body, §2.2,
    `unreachable!` host arm — the four-level walk recovers tables by their
    low physical address, so it is only valid bare-metal). The shared
    `shatter_huge_into` helper copies the leaf's `USER`/`NO_EXECUTE` bits onto
    every sub-entry + the new table pointer, so the split is an attribute-
    faithful, break-before-make-free, idempotent re-expression that fails
    closed (`Misaligned`/`NotMapped`/`PoolExhausted`). Proven end to end by
    the new `tests/integration/stack_guard_qemu_x86_64` (enrolled in
    `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s, the sibling of
    `stack_guard_qemu_{aarch64,riscv64}`): it boots the production
    `rustos-kernel` pipeline (so the GDT, the dedicated error-code-aware `#PF`
    entry, and the bump heap are installed) and, on `BootCompleted`, builds a
    4 GiB-identity space, activates it (`CR3`), `split_block`s the 2 MiB huge
    page covering a dedicated guard static (reached through its low-identity
    physical alias, distinct from the higher-half RIP/stack), proves the split
    preserved the mapping (sentinel write/read-back), then `unmap`s +
    `flush_page`s the single guard page and reads it → supervisor not-present
    `#PF` (`fault` observer confirms `is_not_present` + `!is_user` + `CR2` in
    the guard page) → PASS. **Verified green under QEMU on this host.** The
    production runtime fault-form is proven on x86_64 too (G3c below), and
    both production seams now run on the arena (G3b-2-iii). **No ABI
    change.** Doc: `docs/src/platform/x86_64.md` ("Block split + guard-page
    fault-form (G1/G2)").
  - **x86_64 production guard-page fault-form (G3c) `[x]`.** The cross-port
    sibling of `stack_overrun_qemu_aarch64`: proves an *overrunning kthread*
    on x86_64 takes a **synchronous, supervisor-mode not-present `#PF` while
    running** under the live scheduler — the production runtime payoff, not
    the deferred next-reschedule poison-canary detection a heap-backed
    `BoxStack` falls back to. The new
    `tests/integration/stack_overrun_qemu_x86_64` (enrolled in
    `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s) boots the
    production `rustos-kernel` pipeline (GDT + dedicated error-code-aware
    `#PF` entry + bump heap) and, on `BootCompleted`, builds a 4 GiB-identity
    space, activates it (`CR3`), re-expresses a 2 MiB-aligned guard arena at
    4 KiB granularity (`prepare_guard_arena`, G2), `unmap`s + `flush_page`s
    one kthread stack's one-page guard through the Arch HAL (the production
    mechanism), builds the live `rustos-kernel-sched-eevdf` `Scheduler` over
    `X86_64Arch`, and admits a kthread on that arena stack via
    `spawn_kthread_with_stack` (the **production runtime path**) laid out
    `[guard | usable]` on the arena's low-identity alias. The kthread overruns
    into the unmapped guard page → supervisor not-present `#PF` (the `fault`
    observer confirms `is_not_present` + `!is_user` + `CR2` in the guard page)
    → PASS; a body that returns without faulting drains the `step` loop and
    fails loudly (§2.9). **Verified green under QEMU on this host.** This
    proves the *mechanism*; the production `x86_64::init_spawn` /
    `x86_64::spawn_producer` kernel stacks now run on the boot-reserved arena
    (G3b-2-iii). **No ABI change.** Doc:
    `docs/src/platform/x86_64.md` ("Proving the overrun fault-form (G3c)").

### X — x86_64 concurrent user mode: timeshare → spawn → wait (P6 cross-port follow-on) `[x]`

**x86_64 now reaches a full concurrent, multi-process user mode** to match
aarch64 (SP2c EL0 timeshare, SP3b/SP4 `spawn`, SP6 `wait`): X1–X4 and the X4
follow-on are all `[x]`, so PID 1 `init` spawns the session, reaps it, and
relaunches it under the live scheduler on x86_64 (`spawn_session_qemu_x86_64`,
3/4). The **riscv64** timeshare sibling is the remaining cross-port follow-on,
a *separate, larger* one deferred behind this arc (see the end of this
section): its `trap.s` runs the handler on the interrupted **user** `sp` with no
`sscratch` kernel-stack swap, so it needs a trap-entry redesign before a
cooperative mid-handler park can work at all.

The X1–X4 chunks (below) were staged lowest-risk-first because the x86_64
machinery already existed: ring-3 entry (`rustos_arch_x86_64::userentry`), the
`mem_map` producer path (`mem_map_qemu_x86_64`), a `syscall`/`sysret` stub that
already switches to a kernel stack, an `X86_64Arch: SchedulerArch`, and an
x86_64 `ContextSwitchHal`.

**Binding design findings** (from the x86_64 `syscall` stub,
`kernel/arch/x86_64/src/syscall_entry.rs::syscall_entry_stub`):

- The stub loads the kernel stack from the **per-CPU** `SyscallTls.kernel_rsp0`
  (`gs:0`) and saves the user `%rsp` into the **per-CPU** `user_rsp_save`
  (`gs:8`); the saved user RIP (`%rcx`) and RFLAGS (`%r11`) are **pushed onto
  the kernel stack** (already frame-resident, so they survive a park).
- Unlike aarch64 — where an EL1 trap implicitly reuses the running kthread's
  `SP_EL1`, so each user kthread's syscall lands on its own kernel stack with
  no extra work — x86_64 must **explicitly** point `kernel_rsp0` at the
  **current** user-kthread's own kernel stack on each resume, or two tasks'
  syscall handlers collide on one stack (a correctness *and* isolation defect,
  §4).
- The per-CPU `user_rsp_save` (`gs:8`) is the x86_64 analogue of the aarch64
  `ELR_EL1`/`SPSR_EL1`/`SP_EL0` errata (4c780bc): a task parked **mid-handler**
  by a cooperative `yield`/`wait` (SP2) has its saved user `%rsp` overwritten
  by another task's syscall before it resumes, so the durable save must move
  onto the **per-task kernel-stack frame** (where `%rcx`/`%r11` already live).
  This is a real structural fix — never a limit bump or a "works for one task"
  shortcut (§2.17 / §2.1).

**Security / correctness / performance invariants (all chunks).** Every
syscall stays capability-checked **kernel-side** in `kernel/syscall` (§5.4);
none of this adds authority. Task isolation is enforced by distinct top-level
page tables (a fresh PML4 per space, §4) reactivated through CR3 on resume.
The fixes are structural, fail-closed (§2.9), and carry no `unsafe` without a
`// SAFETY:` block + a test (§2.10). The per-resume CR3 + `kernel_rsp0` reload
is the minimal switch cost the aarch64 sibling already pays; no allocation or
copy is added on the syscall hot path (§2.16).

- **X1 — x86_64 single resumable user-kthread `[x]`.** A single ring-3 task
  is admitted as a resumable user kthread and cooperatively parks/resumes under
  the live scheduler on x86_64. Two primitives, the siblings of the aarch64
  `activate_user_root`: `rustos_arch_x86_64::paging::activate_user_root(root_phys)`
  reloads CR3 (free `mov cr3`, host no-op; the load flushes non-global TLB
  entries so no `invlpg`), and `syscall_entry::set_kernel_rsp0(cpu, top)`
  repoints only the per-CPU `SyscallTls.kernel_rsp0` field (no MSR rewrite, no
  `user_rsp_save` touch) after the same fail-closed `validate_kernel_rsp0`
  stack-pivot check. The arch-neutral `kernel/core::kthread` `pre_resume` hook
  now takes the task's own kernel-stack top (`PreResume = FnMut(u64)`; the
  dispatcher passes `stack.top()`) — closing the gap aarch64 fills implicitly
  via `SP_EL1` (§2.4, not interface creep). The aarch64 hooks ignore the arg.
  Proven by `tests/integration/spawn_el0_resume_qemu_x86_64`: boots the
  production pipeline, builds one isolated ring-3 space via `spawn_image`,
  admits it via `spawn_user_kthread` whose `pre_resume` reloads CR3 +
  `set_kernel_rsp0`, drives `Scheduler::step`, and PASSes once the task yielded
  its full count and exited (dispatch maps `yield`/`exit` to
  `reschedule_current`). The durable user-`%rsp` `gs:8` hazard is **not**
  exercised by one task; its structural fix lands with its two-task exerciser
  in X2. Host tests cover `set_kernel_rsp0`'s validation; docs in
  `docs/src/platform/x86_64.md` ("Resumable ring-3 user kthread").

- **X2 — x86_64 return-state survives a concurrent park + two-task EL0
  timeshare `[x]`.** Two x86_64 tasks timeshare one CPU as resumable user
  kthreads, proven by `tests/integration/spawn_el0_timeshare_qemu_x86_64` (the
  SP2c sibling: two hardware-isolated ring-3 spaces — two PML4s, one shared
  frame pool, §4 — each admitted as a resumable user kthread whose `pre_resume`
  reloads its CR3 + `kernel_rsp0`, driven by the cooperative `step` loop
  mapping each `yield`/`exit` to `reschedule_current`; PASS once both yielded
  their full count and exited). It required **two** independent structural
  fixes, both shipped here (a one-task X1 run exposes neither):
  - **(1) Durable user-`%rsp` save on the per-task kernel frame.**
    `syscall_entry_stub` now `pushq %gs:8`s the just-stashed user `%rsp` onto
    *this task's* kernel-stack frame (beside the frame-resident `%rcx`/`%r11`)
    and restores it with a single `popq %rsp`. The user-`%rsp` slot doubles as
    the System V alignment pad, so the frame size — hence alignment — is
    unchanged (no hot-path cost). `gs:8` is now a transient temp held only
    between the entry `swapgs` and the first kernel-stack push, before any
    cooperative switch can occur, so a task parked mid-handler no longer has
    its saved user `%rsp` clobbered by a *different* task's syscall through the
    shared per-CPU slot. The x86_64 analogue of the aarch64
    `ELR_EL1`/`SPSR_EL1`/`SP_EL0` errata (4c780bc); structural, never a limit
    bump (§2.17).
  - **(2) `swapgs` balance across a cooperative mid-handler park (the blocker,
    not anticipated by the original X2 text).** Fix (1) is necessary but **not
    sufficient**: the two-task vertical (and the *original* pre-(1) stub)
    double-faults identically — a `v=08` #DF with `rsp=0` at
    `syscall_entry_stub`, CR2=-8 — because the per-CPU GS-swap state is left
    unbalanced across a park. The kernel's convention outside the stub's
    swapgs window is current GS = user value, `KERNEL_GS_BASE` = kernel TLS
    (`enter_user` relies on it). When task A's `syscall` runs the entry
    `swapgs` then parks mid-handler via `reschedule_current`, the dispatcher
    enters task B through `enter_user`/`iretq` (no `swapgs`), so B runs ring-3
    with kernel GS still active; B's first `syscall` `swapgs` flips GS the
    wrong way → `movq %gs:0,%rsp` reads address 0 → push faults → #PF on a
    null stack → #DF. X1 never exposes it because the same task always does the
    matching exit `swapgs`. Fix: a HAL cooperative-park hook pair on
    `rustos_arch_api::ContextSwitch` — `enter_cooperative_park` /
    `leave_cooperative_park`, default no-op (aarch64/riscv64 need nothing) —
    that `kernel/core`'s kthread runtime calls in `suspend_thunk` around the
    suspend switch (the user-kthread mid-handler park path). x86_64 implements
    them as a `swapgs` back to the between-handler convention immediately before
    the park and back into the stub-window convention immediately after resume;
    both are on the *task's* control flow and pair exactly, and the first
    trampoline→`enter_user` entry never goes through `suspend_thunk`, so it
    correctly does no swapgs. Structural, fail-closed, no limit bump (§2.17),
    capability checks unchanged (§5.4), per-PML4 isolation intact (§4). No ABI
    change. Stub rustdoc + the `SyscallTls` (transient-`gs:8`) docs updated;
    the host stub-layout test still pins the 16-byte two-word layout. Docs in
    `docs/src/platform/x86_64.md` + `docs/src/architecture/multitasking.md`.

- **X3a — x86_64 PID 1 (`init`) reaches ring 3 (production path) `[x]`.** The
  prerequisite for the x86_64 `spawn` producer: the **production**
  `rustos_kernel::boot` pipeline now spawns PID 1 into ring 3 through the real
  `kernel_main` + `InitSpawn` path (not a test-driven ad-hoc scheduler like
  X1/X2), the cross-port sibling of the aarch64 P6c-3 milestone. Three pieces,
  all wired into `BootInfo`:
  - `x86_64::init_spawn::X86_64InitSpawn` (`with_init`): builds `init`'s ring-3
    image through the audited `spawn_image` and admits it as a resumable user
    kthread (`admit_init`); `pre_resume` reloads CR3 (`activate_user_root`) +
    repoints the entry stack (`set_kernel_rsp0`); `BoxStack` kernel stack
    (software canary — the hardware guard-page form is aarch64-only).
  - `serial_sink::Com1Console` (`with_console`): the COM1 `ConsoleWrite` stream
    backing, so `init`'s fd-1 banner lands (§20); the x86_64 `UartConsole`
    sibling.
  - `boot::try_boot` enables `IA32_EFER.NXE` (production W^X step, §19.2).
  - **Key invariant:** the seam switches CR3 to the fresh space to build the
    image, and the x86_64 page-table walk dereferences tables by their **low
    physical address**, so the space must identity-map all of RAM (not the
    32 MiB `new_identity_first_32mib` window the X1/X2 verticals use). The new
    `paging::AddressSpace::new_identity_first_gib` (shared `new_identity`
    helper) maps 4 GiB, mirroring the boot trampoline (covers RAM + the LAPIC).
    Embedded program rxes now build for x86_64 too (`build.rs` generalised over
    a per-target link recipe). Proven by `tests/integration/spawn_init_qemu_x86_64`
    (PASS on `ProcessSpawned` + an audited `SyscallInvoked`). **No ABI change.**

- **X3b — x86_64 `spawn` concurrent producer `[x]`.** The real x86_64
  `ProcessSpawn` producer — `kernel/rustos-kernel/src/x86_64/spawn_producer.rs`,
  the cross-port sibling of the aarch64 `spawn_producer.rs` — is wired through
  `BootInfo::with_spawn` (in `boot::try_boot`, beside the X3a `with_init` seam)
  with the shared embedded `spawn_layout::PROGRAM_REGISTRY` (the `Shell` `rxe` `build.rs`
  already bakes for x86_64). On `init`'s `CAP_PROC_SPAWN`-gated `spawn` for
  `/Apps/Shell.app/Run`, it draws the child's page tables from the kernel's
  live `FrameAllocator` through a boot-cached `kernel/mem` `FrameTableSource`
  (§24.1 — no fixed `.bss` reserve, capacity scales with RAM, fail-closed
  `NoSpace` only on genuine OOM), builds a 4 GiB-identity child PML4 with
  `new_identity_first_gib`, drives the audited `spawn_image` + `admit_process`
  (the child gets only `{CAP_CONSOLE_WRITE, CAP_CONSOLE_READ}`, no ambient
  authority), and admits
  it **Ready** — returning the PID without entering it (a true concurrent spawn).
  **Key decision:** unlike the X3a PID-1 seam (which switches `CR3` to build the
  image), the producer runs under PID 1's own `CR3` — whose
  `new_identity_first_gib` map covers the low 4 GiB identity (existing-table
  physical derefs + the allocator's page-table and image frames) **and** the
  higher-half kernel window (new-table static pointers + the `DirectPhysMap`) —
  so it builds the child's tables **without switching `CR3`**, never moving the
  running parent out from under itself, exactly as the aarch64 producer builds
  through its identity window (§2.2). The child's own `CR3` is reloaded by its
  `pre_resume` hook (CR3 + `set_kernel_rsp0`); its kernel stack is a software-
  canary `BoxStack` (the hardware guard-page fault-form is aarch64-only —
  riscv64/x86_64 `Pending`). Proven by
  `tests/integration/spawn_session_qemu_x86_64` (enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`): PASS on two `ProcessSpawned`
  (PID 1 + the session) and two audited `SyscallInvoked` — the second necessarily
  the session's `exit`, since `init`'s `wait` only completes after the session is
  reaped, proving the session actually *ran* in its own ring-3 space.
  `init`'s `wait`→reap→relaunch supervision cycle is **not** asserted here — it is
  the x86_64 `wait` validation (X4). **No ABI change.**

- **X4 — x86_64 `wait` sibling `[x]`.** The `KernelProcessWait` producer is
  already installed on every production pipeline by `kernel/core`'s `run_phases`
  (so `register_child` on the spawn-admit path and `record_exit` in `exit`
  fire), so X4 added the proving vertical:
  `tests/integration/wait_qemu_x86_64` (enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`), the cross-port sibling of
  `wait_qemu_aarch64`. It boots the production `rustos-kernel` pipeline (GDT
  ring-3 selectors / TSS / `syscall` entry), and on `BootCompleted` builds a
  **parent** and a **child** hardware-isolated ring-3 space (two PML4s, one
  shared frame pool) from the cross-arch `wait_program` fixture (built PIE in
  both roles + converted to `rxe`), installs a `KernelProcessWait<X86_64Arch>`,
  registers the link, and drives the cooperative `step` loop. It admits the
  **parent first**, so the parent's `wait` runs while the child is still
  registered-but-unexited and the producer **parks** it (`Reap::Blocked` →
  `reschedule_current`); the child then runs, exits, and the parent is
  re-dispatched to reap it and copy the reaped code out to its `status` pointer
  — exercising the resume-after-cooperative-park return-state path on the x86_64
  trap, then exiting 0. PASS verified under QEMU on `-M`/multiboot. The
  resume-after-park path the X4 note flagged is therefore **proven sound on
  x86_64** (X1/X2's durable user-`%rsp` save + `swapgs` balance cover it). **No
  ABI change.**

- **X4 follow-on — x86_64 `init` supervision cycle (relaunch-`spawn`) `[x]`.**
  `spawn_session_qemu_x86_64` now asserts the **full** `wait`→reap→relaunch
  supervision cycle (**3** `ProcessSpawned` / **4** audited syscalls, the
  cross-port equal of the aarch64 sibling): PID 1 `init` spawns the session,
  `wait`s, the session exits, `init`'s `wait` reaps it and returns to ring 3,
  and `init`'s relaunch `spawn` builds a third process — proven green under QEMU.

  **Root cause (was a frame-allocator-vs-kernel-image overlap, not a trap-state
  bug).** The x86_64 `boot::build_memory_map` built the `BootMemoryMap` straight
  from the UEFI map, where `bootmemory::from_uefi` (correctly) classifies
  `EfiLoaderCode`/`EfiLoaderData`/`EfiBootServicesCode`/`Data`/
  `EfiConventionalMemory` as `Usable` — but GRUB loads *this* kernel into that
  memory, and **nothing reserved the running kernel image** (unlike aarch64
  P6c-1's `[ram_base, __kernel_end)`). By the 2nd (relaunch) `spawn`, the low
  usable RAM consumed by boot + PID 1 + the 1st session pushed the allocator
  cursor across 1 MiB into the kernel image; `spawn`'s `build_process_image`
  zero-fill / page-table writes (through the higher-half direct map) corrupted
  live `.text` (the derail target was `0xffffffff80120000` = physical
  `0x120000`, the kernel image), producing the wild CPL=0 execution.

  **Fix (structural, with regression tests).** `kernel/arch/x86_64/linker.ld`
  now emits a `__kernel_phys_end` physical symbol (end of `.bss`, incl. the bump
  heap), `BootMemoryMap::reserve_range` clips a physical range out of every
  `Usable` region (preserving the allocator's no-overlap invariant; leaving the
  range an implicit reserved gap), and `build_memory_map` reserves
  `[__boot_phys_start, __kernel_phys_end)`. Host-tested in
  `kernel/mem/src/bootinfo.rs` (split/truncate/skip/zero-width + an
  allocator-never-hands-out-a-reserved-frame contract test); proven end to end
  by the strengthened `spawn_session_qemu_x86_64` (3/4). Docs:
  `docs/src/platform/x86_64.md` ("Reserving the kernel image out of usable
  RAM").

**Done when (per chunk):** the chunk's QEMU vertical PASSes under `cargo xtask
test --qemu` **and** the whole-project gate (§5) is green; docs + host tests
land in the same change (§7 / §13).

**riscv64 concurrent user mode `[x]`.** The riscv64 spawn/wait
timeshare sibling of the x86_64 X-series, landed lowest-risk-first, one
fully-gated chunk per landing. All of RV1–RV-X4 are done: the riscv64 port
now brings up concurrent, multi-process user mode (resumable user kthreads,
two-task timeshare, runtime `spawn`, and blocking `wait`/reap), reaching
parity with the aarch64 and x86_64 ports.

- **RV1 — `trap.s` per-task kernel stack + frame-resident return state
  `[x]`.** The prerequisite trap-entry redesign. The vector now swaps `sp`
  with `sscratch` on entry (port invariant: `sscratch` = this hart's current
  user task's kernel-stack top while in U-mode, 0 while in S-mode; a nested
  S-mode trap lands `sp == 0` and is recovered onto the interrupted kernel
  `sp`), so the handler never runs on the interrupted **user** `sp` (which a
  cooperative `ContextSwitch::switch` taken mid-handler would wrongly persist).
  It saves `sepc`/`sstatus`/the interrupted `sp` into an enlarged 160-byte
  `trap::TrapFrame` (GP-register offsets unchanged, so the `[u64; …]` syscall
  view is intact) and reloads them before `sret`, picking the U-mode vs S-mode
  return path from the saved `sstatus.SPP`; the syscall path advances the
  **saved** `frame.sepc`. `userentry::enter_user` arms `sscratch` before its
  first `sret`; `init_traps` zeroes it at boot. This is the riscv64 sibling of
  the aarch64 `ELR_EL1`/`SPSR_EL1`/`SP_EL0` return-state errata. Host-proven
  (the `TrapFrame` `offset_of!` asserts pin the layout against `trap.s`) and
  every line of the redesigned vector is exercised by the existing riscv64
  matrix: U-mode `ecall`s/faults (`mem_map`/`spawn_program`/`abi_sys`/
  `memory_isolation`) drive the from-U swap + U-return path, and S-mode
  timer/IPI traps (`sched_drive`/`ipi_smp`/`timer_preempt`) drive the nested-S
  recovery + S-return path — all green under QEMU. No ABI/C-header impact
  (`TrapFrame` is internal to the arch crate). Doc:
  `docs/src/platform/riscv64.md` ("Per-task kernel stack + frame-resident
  return state").
- **RV-X1 — single resumable user-kthread `[x]`.** The riscv64 sibling of
  x86_64 X1 / aarch64 SP2b. `rustos_arch_riscv64::paging::activate_user_root(
  root_phys)` is the per-task `pre_resume` reactivation primitive: it
  reprograms `satp` (`satp_sv39(root_phys)` + `sfence.vma`) on a hart whose
  paging is already on — a free function over the raw `u64` root (so the hook
  stays `Send`), lighter than `AddressSpace::switch`, with a bare-metal arm and
  an inert host arm presenting one `unsafe` API. The
  `tests/integration/spawn_el0_resume_qemu_riscv64` vertical (reusing the
  arch-neutral `el0_yielder` fixture) reads the timer rate from the firmware
  tree, builds one isolated Sv39 U-mode space via `kernel_core::spawn_image`,
  admits it as a **resumable user kthread** via `spawn_user_kthread` (its
  `pre_resume` hook calls `activate_user_root`; the handed kernel-stack top is
  unused on riscv64 — `sscratch` is armed by `userentry::enter_user` and
  preserved across a park by RV1, with per-task `sscratch` repointing deferred
  to RV-X2), and drives the cooperative `Scheduler::step` loop while the
  dispatch callback maps each `yield`/`exit` `ecall` to `reschedule_current`.
  PASS once the task yielded its full count and exited — the first chunk that
  *exercises* RV1's mid-handler-park safety on a user task. Doc:
  `docs/src/platform/riscv64.md` ("Resumable U-mode user kthread").
- **RV-X2 — two-task EL0 timeshare `[x]`** (SP2c sibling).
  `tests/integration/spawn_el0_timeshare_qemu_riscv64` proves **two**
  hardware-isolated U-mode tasks timeshare one hart as resumable user kthreads
  on `-M virt`: two Sv39 spaces (two `PageTablePool`s + a shared frame pool, §4)
  built from the one `el0_yielder` `rxe` via `kernel_core::spawn_image`, each
  admitted via `spawn_user_kthread`, driven by the cooperative `Scheduler::step`
  loop with the dispatch callback mapping each `yield`/`exit` `ecall` to
  `reschedule_current`. **No new structural code was needed** (the vertical
  only): unlike x86_64's per-CPU `set_kernel_rsp0`, riscv64 `sscratch` is
  per-task hardware state — `userentry::enter_user` arms it on first entry and
  the RV1 trap vector re-arms it from each task's own kernel-stack frame on
  every U-return (`trap.s`: `sscratch = sp + TRAP_FRAME_SIZE`), so each
  `pre_resume` hook only reactivates its `satp` root and ignores the
  kernel-stack-top argument (the predicted per-task `sscratch` repointing is
  unnecessary, as aarch64 SP2c needed nothing over SP2b). Doc:
  `docs/src/platform/riscv64.md` ("Two-task U-mode timeshare (RV-X2)").
- **RV-X3 — `spawn` concurrent producer `[x]`** (SP3b/SP4 sibling).
  `tests/integration/spawn_session_qemu_riscv64` proves a parent U-mode
  task's `CAP_PROC_SPAWN`-gated `spawn` builds a fresh, hardware-isolated
  Sv39 child and admits it **Ready** concurrently on `-M virt`. The
  `spawn_session_program` fixture is one source in two roles (parent
  `spawn`s the session then yields; child/session yields then exits,
  `AGENTS.md` §2.2). The mini-kernel admits the parent as a resumable user
  kthread; its `spawn` `ecall` is routed by the dispatch callback to a
  riscv64 `ProcessSpawn` producer (the cross-port equal of
  `Aarch64ProcessSpawn` / the x86_64 producer) that builds the child its
  own Sv39 space over a separate `PageTablePool` (data frames from the same
  monotonic pool, never aliasing, §4) **through the parent's identity
  window without switching the running parent's `satp`**, admits it Ready
  via `spawn_user_kthread`, and returns its PID — the parent keeps running
  (a true concurrent spawn). The child's own root is installed by its
  `pre_resume` hook (`activate_user_root`) on first resume. PASS once the
  producer built the child and both tasks yielded their full count and
  exited (two `ProcessSpawned`). Doc: `docs/src/platform/riscv64.md`
  ("Runtime `spawn` concurrent producer (RV-X3)").
- **RV-X4 — `wait` `[x]`** (SP6 sibling): the riscv64 cross-port equal of
  `wait_qemu_aarch64` / `_x86_64`. `tests/integration/wait_qemu_riscv64`
  proves a parent U-mode task `wait`s on its spawned child, parks until the
  child exits, reaps it, and reads back its code on `-M virt`. It reuses the
  arch-neutral `wait_program` two-role fixture (child exits with a
  build-pinned code; parent `wait`s + verifies), builds a child + parent as
  isolated Sv39 spaces (the RV-X3 mini-kernel shape) via
  `kernel_core::spawn_image`, installs the shared
  `kernel_core::KernelProcessWait<RiscvArch>` producer, registers the
  parent→child link, and drives the cooperative `step` loop: the child
  `exit`s, the parent's `wait` parks (`reschedule_current`, no busy-spin §2.1)
  then reaps it, the kernel copies the reaped code out to the parent's
  `status` through the retained frozen parent space (`copy_out`), and the
  parent verifies it and exits 0 (PASS, ids 4332-4334). The first riscv64
  exerciser of the resume-after-cooperative-park return-state path on a
  *user* task (RV1's per-task kernel stack + frame-resident return state).
  No ABI change. Doc: `docs/src/platform/riscv64.md` ("`wait`: blocking reap
  of a child (RV-X4)").

**riscv64 production boot path (RV-P series).** The riscv64 spawn/wait
arc above proved the concurrent-user-mode *mechanism* in test mini-kernels;
the RV-P series brings the **production `rustos-kernel` binary** up on
riscv64, mirroring the aarch64 P-stage arc.

- **RV-P1 — production boot to `BootCompleted` `[x]`.** The production
  `rustos-kernel` binary now boots the QEMU `virt` / SiFive board
  (`riscv64gc-unknown-none-elf`, linked with the arch port's
  `riscv64-virt.ld`) to `AuditEvent::BootCompleted`. The boot pipeline is
  the new `rustos_kernel::boot_riscv64` (`RiscvBinArch` `KernelArch`
  adapter, `build_boot_memory_map`, `try_boot`, `boot`): it parses the
  OpenSBI-handed device tree for the RAM window + `timebase-frequency`,
  builds the two-region `BootMemoryMap` (`[ram_base, __kernel_end)`
  reserved, the page-aligned remainder usable), and hands a validated
  `kernel_core::BootInfo` to `kernel_core::kernel_main` with `satp = 0`
  (the `virt` board's atomics are well-defined MMU-off, so no Sv39 bring-up
  is needed to reach `BootCompleted`). This pipeline is the **single**
  riscv64 boot orchestration (§2.2): the `tests/integration/riscv64_boot`
  wrapper re-exports it and only adds the test-side firmware-map/DTB
  observers before delegating, so every riscv64 QEMU vertical
  (`kernel_arch_boot_riscv64`, the virtio/framebuffer/input bins) runs the
  production code. Proven by `kernel_arch_boot_riscv64` (`id=4004 kernel
  boot completed` → SiFive PASS). **No ABI change** (the `lib/abi` types,
  syscall table, and C header are untouched). Doc:
  `docs/src/platform/riscv64.md` ("Kernel boot pipeline").
- **RV-P2 — Sv39 MMU enable + trap vector + syscall dispatch `[x]`.** The
  production `boot_riscv64::boot` now runs **paged**:
  `enable_mmu_and_vectors` identity-maps the whole low Sv39 window
  (`[0, 512 GiB)`, 1 GiB leaves over a `.bss` `PageTablePool`), writes
  `satp`, and points `stvec` at the S-mode trap vector via the new
  `trap::install_trap_vector` (the vector-only half factored out of
  `init_traps`, so the boot installs the vector **without** enabling
  asynchronous interrupts — `sie`/`sstatus.SIE` stay clear). The
  production `ecall` dispatch callback `riscv64::dispatch::production_dispatch`
  (the riscv64 sibling of `x86_64::dispatch`/`aarch64::dispatch` over the shared
  `dispatch_core`) is installed before any user thread can run; a pool that
  cannot satisfy the identity map fails closed. Because the map is identity
  (physical == virtual) and full-window, every board address — kernel
  image, DTB, PLIC, MMIO, the device-bring-up DMA carves — keeps its
  address under translation, so every riscv64 vertical runs under the paged
  boot. Proven by `kernel_arch_boot_riscv64` (`mmu_enabled=true
  dispatch_installed=true` → `id=4004` → SiFive PASS) and the
  virtio-blk/net + framebuffer verticals (device bring-up MMU-on). **No ABI
  change.** Doc: `docs/src/platform/riscv64.md` ("Kernel boot pipeline").
- **RV-P3 — user-mode drop + kthread-spawning seam `[x]`.** The riscv64
  `InitSpawn`/`ProcessSpawn` production seams (`riscv64::init_spawn` /
  `riscv64::spawn_producer`, the aarch64 `init_spawn`/`spawn_producer`
  analogue) are installed by `boot_riscv64::try_boot` via
  `BootInfo::with_init`/`with_spawn`, alongside the SBI-console
  `with_console` backing (`RiscvUartConsole` over the new verbatim
  `serial::write_console_bytes`). After `BootCompleted`, `kernel_main`
  drops PID 1 `init` into U-mode (its own Sv39 root, `IDENTITY_GIB = 4`,
  an arena-backed hardware-guarded kernel stack since G3b-2-iv), `init`
  writes its banner through `stream_write` and
  issues the `CAP_PROC_SPAWN`-gated `spawn` for `/Apps/Shell.app/Run`; the
  producer builds the session a fresh, hardware-isolated space from the
  allocator-backed `FrameTableSource` (no fixed reserve, §24.1) and admits
  it Ready. The kernel `build.rs` now also builds the embedded `init`/`Shell`
  `rxe` blobs for the riscv64 target. Proven by `spawn_init_qemu_riscv64`
  (`id=4030` PID 1 → `RustOS init: reached user mode` banner → `id=4030`
  Shell → `id=5000 sc=spawn` → SiFive PASS). **No ABI change.** Doc:
  `docs/src/platform/riscv64.md` ("PID 1 into user mode").

### P7 — VideoCore mailbox + framebuffer (metal) `[~]`

**Landed — the host-provable protocol half.** The BCM2711 mailbox
property-channel client lives in the shared `lib/vcmailbox` crate
(§2.2 — the P7b framebuffer boot console speaks the same protocol; doc:
`docs/src/drivers/display.md`, "Firmware framebuffer discovery"):

- `FramebufferRequest::encode` frames the framebuffer request (set
  physical/virtual size, depth 32, pixel order from the
  `DisplayFormat`, allocate at page alignment, get pitch);
  `decode_framebuffer_response` validates the in-place answer
  fail-closed (header code, per-tag response bits/lengths, exact
  geometry echoes, pitch/size consistency); `bus_to_arm_physical`
  strips the 2-bit VC alias and rejects a zero, unaligned, or
  out-of-aperture buffer. The decoded `FirmwareFramebuffer` yields the
  `ScanoutConfig` (`ScanoutConfig::from_firmware`, plus the bus alias)
  `RpiHvs::open` consumes; the crate also carries the display-size
  query (`query_display_size`) the P7b boot console probes with.
- The doorbell is behind the `MailboxTransport` seam: `MmioMailbox`
  drives the register block over two capability-gated `RegisterWindow`s
  with a budget-bounded poll (`DEFAULT_POLL_BUDGET`), failing closed
  with `Timeout`/`MalformedResponse` (foreign completion) — never an
  unbounded spin.
- Host tests cover framing (framebuffer + display-size), every
  fail-closed decode path, the alias↔aperture translation in both
  directions (`bus_to_arm_physical` / `arm_physical_to_bus`), and the
  doorbell transport in `lib/vcmailbox` (against its shared
  `mock::MockFirmware`, exported behind the `mock-firmware` feature),
  plus the wiring fail-closed paths and the full chain in `rpi_hvs`:
  mock firmware → `wiring::open_with_transport` → `ScanoutConfig` →
  `RpiHvs::open` → `present` into the discovered surface.
- **No QEMU vertical, deliberately.** `virt` RAM begins at
  `0x4000_0000` — outside the BCM2711 30-bit VideoCore aperture — so
  the driver's (correct) aperture validation can never pass there, and
  §0.4 forbids a Pi-board QEMU vertical. The emulation artefact is the
  host-side full-chain test; the real scan-out is the metal item below.

**Landed — the metal wiring.**

- `FdtDiscovery` discovers the mailbox node (`brcm,bcm2835-mbox`) and
  emits it into `rustos_abi::hwtree` through the generic Stage 4.HW
  walk (`kernel/arch/aarch64::platform`): the doorbell window as a
  capability-gated MMIO resource (base/length read from the tree, never
  a `const`) plus the one per-device augmentation — a `HwResource::dma`
  request for a one-page property-buffer carve bounded by the 30-bit
  VideoCore aperture (the `lib/fdt` `raspi_like_arm` fixture carries
  the node). The QEMU `virt` tree has no mailbox, so its hardware tree
  simply omits the node (§18.4) and the `-M virt` verticals are
  untouched.
- `drivers/display/rpi_hvs::wiring` is the driver-host bring-up seam:
  `open_discovered` checks `CAP_MMIO_MAP`, maps the discovered doorbell
  + the host's property-buffer carve, translates the carve to a bus
  address (`arm_physical_to_bus`), rings `MmioMailbox`, and delegates
  to `open_with_transport`, which assembles the full `HvsConfig`
  (firmware scan-out + the host's `HvsRegions`: DLIST RAM, control
  window, plane carves) and calls `RpiHvs::open`.

**Remaining — metal acceptance.**

- Metal bring-up checklist (record each step's UART log): boot the P6
  image with HDMI attached → `MmioMailbox` exchange returns the
  firmware framebuffer (log bus address, size, pitch) →
  `bus_to_arm_physical` + map → `RpiHvs::open` + clear the frame to the
  theme colour → capture the photo + UART log as the acceptance
  artefact.

**Done when:** the mailbox property protocol has host unit tests
(request/response framing, bus↔physical translation, fail-closed on a bad
aperture) — done; `rpi_hvs` consumes a discovered `HvsConfig` — done
(hardware-tree mailbox node + `wiring::open_discovered`); and a metal
bring-up checklist + a captured "framebuffer cleared to theme colour"
photo/UART-log is recorded as the acceptance artefact — pending metal.

### P7b — Framebuffer boot console: video first, UART fallback `[~]`

Console output (boot log and every later phase) defaults to the
**attached display**; the UART is the last resort when no video output
exists. Doc: `docs/src/platform/aarch64.md`, "Framebuffer boot
console".

**Landed — the code-complete console.**

- `kernel/arch/aarch64::video`: `find_mailbox` discovers the
  `brcm,bcm2835-mbox` doorbell with the shared early-returning
  `fdt::scan_translated` walk; `bring_up` (over the `lib/vcmailbox`
  `MailboxTransport` seam) queries the display's EDID-derived native
  size (`0×0` = no display → UART keeps the console) and allocates a
  32-bit surface at exactly that size; `Geometry`/`TextConsole` render
  the shared `rustos_font::glyphs` 5×7 atlas (§2.2) at an integer
  scale (`height / 360`, clamped 1…4) on a ring grid (wrap-to-cleared
  top row — no megabyte scroll copies per log line, §2.16).
- Bring-up runs in the **pre-MMU** phase of
  `rustos-kernel::boot_aarch64` (caches off ⇒ the property exchange is
  DMA-coherent with no maintenance; the state cell is written by the
  single-threaded boot CPU — no atomic RMW MMU-off). Post-MMU rendering
  serialises on a private DAIF-masking spinlock (not `lib/sync`: feature
  unification across the aarch64-none test-matrix build would force its
  alloc-backed `epoch` into the allocator-free minimal QEMU bins) and
  cleans the touched
  scanlines (`dc cvac` + `dsb`) so the firmware scan-out sees them. The
  doorbell base joins the Device-gigapage mask inputs; the boot audit
  line carries `video_console=true/false`.
- `serial::ConsoleWriter` (log sink) and `serial::write_console_bytes`
  (the `stream_write` fd 1/2 backing) render to the screen when
  `video::is_active`, else fall back to the UART; console input stays
  on the UART. Everything fails closed to the UART (§2.9): no mailbox
  node (QEMU `virt` — the UART verticals are unchanged), detached
  display, or any rejected firmware answer.
- Host tests: fixture mailbox discovery (translated `0xFE00_B880`),
  mailboxless-tree fallback, mock-firmware bring-up (native mode,
  detached display, inconsistent answer), the geometry scale policy and
  fail-closed surface validation, and the renderer (glyph rows,
  `?` fallback, `\n`/`\r`, column wrap, ring-row clear, dirty bands).

**Remaining — metal acceptance.** Boot the SD image with HDMI attached
and capture the boot log **on screen** (photo) plus the
`video_console=true` audit line; with HDMI detached confirm the UART
carries the same log (`video_console=false`).

**Done when:** the boot log renders on the attached display on a real
Pi 4 with the UART fallback proven by the detached-display boot —
pending metal; everything host-provable is landed and tested — done.

### P8 — SD-card storage (EMMC2) `[~]` (read + write paths code-complete; metal pending)

**Depends on `PLAN.md` Stage 4.HW** (bind table + `devmgr` + the drvhost
`.rxe` process-spawn path) — all landed: the aarch64 walk emits a
`brcm,bcm2711-emmc2` node (Storage class, translated MMIO window) from a
Pi-shaped tree with no per-device code, and `devmgr` binds the driver
against its `compatible` string (§18.3).

**Landed — the PIO block driver (read + write).** `drivers/storage/emmc2`
(`rustos-drv-storage-emmc2`) is an Arasan / SDHCI-5.1 PIO block driver
implementing `rustos_abi::driver::block::Block`:

- The SDHCI command/response and block-transfer state machine (`Emmc2`)
  is written against the `SdhciHost` register seam (the one register
  read/write boundary): metal drives it over a capability-gated
  `RegisterWindow` (`SdhciHost` is implemented for it), host tests over a
  register-level mock controller. This mirrors the `rpi_hvs` mailbox seam
  (§2.2) — the protocol layer is proven host-side, the register block on
  metal.
- `Emmc2::open` runs the standard SD identification (reset → ident clock
  → `CMD0`/`CMD8`/`ACMD41`/`CMD2`/`CMD3`/`CMD9`/`CMD7`/`CMD16`) and
  derives the geometry from the card CSD (`command::geometry_from_csd`,
  CSD v2). Only high-capacity, block-addressed (SDHC/SDXC) cards are
  supported; a byte-addressed, pre-v2, or CSD-v1 card is rejected
  fail-closed (`Unsupported`, §5.4). Transfers are PIO through the
  buffer data port in both directions — `CMD17`/`CMD18` reads,
  `CMD24`/`CMD25` writes — so no DMA capability is needed. Every
  controller wait (including the write buffer-ready wait) is
  poll-budget-bounded and fails closed (`DeviceFault`) rather than
  spinning (§2.1 / §24.4).
- `wiring::open_discovered` is the host bring-up seam: it checks
  `CAP_MMIO_MAP`, maps the discovered register window through the host's
  `MmioMapper` (never a `const` base), and opens the engine over it.
- 23 host tests cover `CMDTM`/CSD decode, full identification + geometry,
  single/multi-block reads and writes (writes read back through the same
  mock card, neighbouring blocks proven untouched), shape/range rejection
  on both paths, the unsupported-card paths, command-error (read and
  write) and stalled-controller fail-closed, and the `wiring` capability
  gate. Docs: `docs/src/drivers/block.md`. **No QEMU vertical,
  deliberately** — QEMU models no Pi EMMC2 controller (§0.4); the
  emulation artefact is the host state-machine test.

**Remaining — metal acceptance.** A metal checklist (boot the P9 image on
a real Pi 4 → read the FAT boot partition and the RustFS root from the
card → capture the UART log as the acceptance artefact).

**Done when:** host unit tests cover the SDHCI command/response + block
transfer state machine (both transfer directions) against a mock host —
done; a metal checklist demonstrates reading the FAT boot partition and
the RustFS root from a real card — pending hardware.

### P9 — Bootable SD image (`tools/mkimage`) `[~]`

The image builder is landed; only the on-metal boot of the emitted image
remains (pending hardware).

- `tools/mkimage` (`rustos-mkimage`, lib + bin) authors
  `images/rustos-aarch64-rpi.img` in pure Rust (§12 — no
  `parted`/`mkfs` shell-outs): an MBR (two 1 MiB-aligned primaries,
  `0x0C` FAT32 boot @ LBA 2048, `0x7F` RustFS root, 64 MiB each), with
  both partitions laid down by the **real** in-tree drivers
  (`Fat32::format` / `RustFs::format` — author and consumer share one
  on-disk definition, §2.2), mirroring the
  `tests/integration/{fat32,rustfs}_image` fixture pattern.
- Boot partition: the verified firmware blobs (the `disable-bt` overlay
  planted at its firmware-fixed `overlays/` path), a generated
  `config.txt` (`arm_64bit=1`, `kernel=kernel8.img`, `enable_uart=1`,
  `dtoverlay=disable-bt`, `init_uart_baud=9600`;
  `armstub=armstub8.bin` only when the optional stub is staged), and
  `kernel8.img` — the P1 release ELF flattened by `mkimage`'s fail-closed
  converter (`elfflat`: ELF64/LE/`ET_EXEC`/aarch64 only, `PT_LOAD` layout
  must start *and* enter at `0x8_0000`, overlap/size-bound checks).
- Firmware blobs stay uncommitted third-party inputs (§19.3):
  `tools/mkimage/firmware.lock` pins the upstream HTTPS `source`
  directory plus name + byte length + SHA-256 of `start4.elf` /
  `fixup4.dat` / `bcm2711-rpi-4-b.dtb` / `overlays/disable-bt.dtbo`
  at upstream release `1.20260521`
  (provenance + licence documented in the manifest); verification fails
  closed on any mismatch. `cargo xtask image` fetches any blob missing
  from its `target/pi-firmware/` cache from the pinned source and gates
  every download on the manifest checksums, so the image build is one
  step; an operator-staged `--firmware` dir is only verified, never
  written. `armstub8.bin` is optional and unpinned — no official binary
  exists and the boot stub parks secondaries itself; it joins the
  manifest when SMP-on-metal needs it.
- Root partition: an encrypted RustFS volume (no plaintext mode) carrying
  the §16 skeleton (`/System` + its twelve subdirectories incl.
  `Security/{Keys,Policy}`, `/Users`, `/Apps`, `/Storage`); the §11
  databases/users are the installer's first-boot job. The volume key is
  **passphrase-derived** (§11): the build provisions a per-volume rustfs
  `UnlockDescriptor` (random salt + PBKDF2 cost), derives the key from
  `IMAGE_PASSPHRASE` (blank for both profiles — these are special-case
  images: the debug image never ships, the installer image is
  re-provisioned on first boot), provisions the root under it, and plants
  the plaintext descriptor on the FAT boot partition as `root.unlock` (the
  LUKS-header analogue the bootstrap reads before mounting). The derived
  key is written to the sibling `…-rpi.rootkey` file (0600) for host
  mounting — never inside the image, and re-derivable from `root.unlock` +
  the blank passphrase. A shippable user root is unlocked by an
  operator-chosen passphrase the installer sets, never a blank default.
- Entry points: `cargo xtask image --target aarch64-rpi` and the
  delegating `cargo xtask build --target aarch64-rpi` (`--headless`
  accepted; the image content is identical until installable GUI userland
  ships). A staged firmware dir may come from `--firmware` or
  `$RUSTOS_PI_FIRMWARE`; otherwise the pinned blobs are fetched
  automatically. The standalone `rustos-mkimage rpi` CLI mirrors the
  same flags (with `--firmware` required — no network I/O in mkimage).
- 38 host tests: MBR encode/validation, ELF→flat layout + every refusal,
  manifest parse/verify fail-closed (incl. the committed manifest),
  boot/root partition round-trips re-mounted through the real drivers,
  the `root.unlock` descriptor planted on FAT re-deriving the exact
  volume key, a wrong-passphrase mount refusal (no separate oracle, §5.4),
  and full-image assembly with both partitions mounted from their MBR
  offsets. Docs: `docs/src/install/raspberry_pi.md`.

**Remaining — metal:** boot the emitted image on a real Pi 4 per the
flashing/first-boot doc and record the UART-log checklist (the P7/P8
metal items ride the same boot).

**Done when:** `cargo xtask build --target aarch64-rpi` (and `--headless`)
produces a flashable `.img` — done; `docs/src/install/raspberry_pi.md`
documents flashing + first boot — done; the image boots P6 (user mode) on
real hardware per a recorded checklist — pending hardware.

### P10 — USB-HID input + desktop on the Pi `[~]`

- Bring up the Pi 4 USB host (VL805 PCIe → xHCI for the USB-A ports, and
  the DWC2 OTG) far enough to enumerate a USB-HID keyboard + mouse under
  `drivers/bus/usb` + `drivers/input`, so the WM input router has real
  events.
- Run `userland/gui/{wm,taskbar,session}` on the HVS path: the headless
  build stays first-class (§17.3), and the graphical session is the
  launchable option `userland/session/login` offers when the display +
  input drivers loaded.

**Landed — the host-provable protocol layers** (the `emmc2`/`rpi_hvs`
seam shape, §2.2; no QEMU vertical — QEMU models no Pi USB timing,
§0.4):

- `drivers/input/usb_hid` (`rustos-drv-input-usb-hid`): HID
  boot-protocol keyboard + mouse report decode (USB HID 1.11 App. B)
  into `rustos_abi` `InputEvent`s behind the `ReportSource` seam,
  which lives in `lib/abi` (`rustos_abi::driver::input`) because its
  producer is the sibling xHCI driver and drivers depend only on
  `lib/*` (§17.4). Stateful report diffing (one `Key` edge per change;
  HID usage IDs, modifiers `0xE0..=0xE7`, buttons `0x110..` matching
  the virtio pointer vocabulary), rollover handling, fail-closed
  length/forged-source validation (§5.4), an event latch so undersized
  `poll` buffers lose nothing, and a per-`poll` report budget (§2.1).
  21 host tests; docs: `docs/src/drivers/input.md`.
- `drivers/bus/usb` (`rustos-drv-bus-usb`, placeholder replaced): the
  xHCI protocol layers and the HID enumeration engine over the
  `XhciHost` register seam (`RegisterWindow` on metal, register-level
  mock in tests) and the `DmaRegion` memory seam (`lib/abi` `DmaSlab`
  on metal, a shared in-memory buffer in tests) — `regs`
  (cap/op/runtime/doorbell vocabulary), `trb` (fail-closed
  `TrbType`/`CompletionCode`, event-field decode, byte conversion),
  `ring` (memory-free `ProducerRing` returning `PushOutcome`s the
  memory owner publishes; borrow-free `EventRingCursor`), `Xhci`
  (§4.2 `open` prologue; `start` programming
  `CONFIG`/`DCBAAP`/`CRCR` + interrupter 0's event ring over `RTSOFF`
  and running the controller; `ack_event`; RW1C-safe `reset_port`),
  and `device::UsbDevice` — the single-device enumeration engine
  (64-byte-aligned layout of all device-shared structures; Enable
  Slot / Address Device / Configure Endpoint command flow; control
  transfers: fail-closed `GET_DESCRIPTOR(device)`,
  `SET_CONFIGURATION(1)`, `SET_PROTOCOL(boot)`; a primed interrupt-IN
  ring) implementing `ReportSource` with end-to-end claim validation
  (slot/endpoint/code/address/residual, §5.4) and retire/re-arm
  across the Link-TRB wrap. 38 host tests against the register-level
  mock plus an in-memory ring model sharing one buffer, including a
  `BootKeyboard` decoding key events over the mock controller and the
  fail-closed paths (forged residual, stalled class request, empty
  port, double enumeration, bad DMA regions); docs:
  `docs/src/drivers/bus.md`.

**Landed — ECAM configuration access** (the cross-arch path the VL805
sits behind): `drivers/bus/pci` gained `mechanism_ecam`, an
`EcamConfigSpace` `ConfigSpace` impl over a capability-mapped
`rustos_abi::RegisterWindow` (PCI Express Base 3.0 §7.2.2 flat
offset, `ConfigAddress::ecam_offset`), fail-closed to the all-ones
"no device" sentinel on an out-of-window or malformed access (§5.4).
The mechanism-agnostic enumeration / capability / BAR core is reused
unchanged (§2.2); host-proven by a flat-ECAM VL805 fixture
(`1106:3483` xHCI behind a root-port bridge) driving enumeration +
MSI-X capability decode, plus offset/round-trip/sentinel unit tests.
Docs: `docs/src/drivers/bus.md`, the crate README.

**Landed — PCIe host-bridge discovery** (the aarch64 `FdtDiscovery`
walk): the `brcm,bcm2711-pcie` node is emitted generically (a `Bus`
node whose controller/config — ECAM-access — `reg` window translates
through the bus `ranges` exactly like every other device), with one
per-device augmentation: the **inbound-DMA aperture** the bridge grants
devices behind it, read from the node's `dma-ranges` by the new
`fdt::dma_ranges_aperture` (the 3-cell child PCI address stepped over,
the 2-cell parent CPU base + size decoded, fail-closed). It is emitted
as `HwResource::dma(top, len)` — `top` the *exclusive* upper bound of
the reachable CPU-physical window (`0xC000_0000`, the low 3 GiB of
SDRAM, on the Pi 4), `len` its extent — matching the mailbox "carve
below `base`" convention; the bases are discovered, never a board
constant (§18.5). Host-proven by the platform discovery tests (a
`/scb`-nested PCIe bridge: translated `reg` window `0xfd50_0000`/`0x9310`
+ aperture `0xC000_0000`; the no-`dma-ranges` fail-closed case) and the
`fdt::dma_ranges_aperture` unit tests (real Pi value, multi-entry span,
absent/partial/out-of-range-cell refusals). **No `lib/abi` change** (so
no C-header regen). Docs: `docs/src/platform/aarch64.md` ("Platform
discovery").

**Landed — the VL805 `wiring::open_discovered`** (host-provable): the
generic-PCI seam `rustos_abi::driver::pci::PciBus` (a supertrait of
`Bus`) carries `map_bar_window` + `enable_bus_master` — the smaller
surface a non-virtio, DMA-driving controller needs (no MSI-X). `Pci<C>`
implements it by forwarding to the inherent BAR resolver and a shared
`enable_bus_master` that `route_msix` also calls (§2.2);
`mechanism_one`/`mechanism_ecam` now return `impl VirtioPciBus + MsixBus
+ PciBus`. `drivers/bus/usb::wiring::open_discovered(host, bus,
dma_aperture_top)` consumes a `&dyn PciBus` (so usb never names the pci
crate, §17.4): it checks `CAP_MMIO_MAP`, enumerates for the USB-class
function (`0x0C03`), carves the device-shared DMA region from the host
DMA facility and verifies it lies wholly below the discovered
inbound-DMA aperture `top` (fail-closed `OutOfRange`, §5.4), enables bus
mastering, maps BAR0, and brings the controller up via `Xhci::open` +
`UsbDevice::start`. **No `#[repr(C)]`/syscall change** — a new trait, so
no C-header regen. Host-proven: pci tests (PciBus coercion,
`enable_bus_master` command bits, BAR0 map, absent-BAR refusal over the
VL805 ECAM fixture) + usb `wiring_tests` (the cap/mapper/DMA-host
fail-closed paths, no-USB-function `NotFound`, DMA-above-aperture
`OutOfRange`, alloc-failure propagation, and the all-valid path enabling
mastering and reaching the controller hand-off — the inert mock window
faults, the metal boundary). Docs: `docs/src/drivers/bus.md`,
`docs/src/abi/driver_traits.md`, the crate README.

**Landed — the BCM2711 PCIe root-complex bring-up** (host-provable): the
VL805 sits behind the BCM2711 root complex, which ships with its link
**down** and whose configuration space is *not* flat ECAM. Two pieces
close that gap. `drivers/bus/pci` gained `mechanism_brcm` +
`BrcmConfigSpace`, the BCM2711 *windowed* (index/data) configuration
access (`EXT_CFG_INDEX` 0x9000 / `EXT_CFG_DATA` 0x8000): the root-bus
header is read directly, a downstream function's `(bus<<20)|(devfn<<12)`
block address is written to the index register, then the dword is reached
through the 4 KiB data window — the only BCM2711-specific knowledge, the
enumeration/BAR/cap core unchanged (§2.2). The new
`drivers/bus/pcie_brcm` crate (`BrcmPcieRc`) performs the link bring-up
over the BCM2711 root-complex registers: reset + assert `PERST#`, power the SerDes
(clear `IDDQ`), program `MISC_CTRL`, the inbound `RC_BAR2` viewport from
the discovered `dma-ranges` (size via `encode_ibar_size`), disable
`RC_BAR1`/`RC_BAR3`, confirm the root-port role (fail closed otherwise),
advertise ASPM + the PCI-PCI bridge class, program the outbound `ranges`
MMIO window, deassert `PERST#`, then poll `MISC_PCIE_STATUS` for link-up
bounded by `DEFAULT_LINK_POLLS` (100 ms, fail closed). Written against a
`PcieRegs` (`RegisterWindow` on metal, register mock in tests) + `Delay`
seam; `wiring::open_discovered` maps the controller window under
`CAP_MMIO_MAP`. **No `lib/abi`/C-header change.** Host-proven: pci
`mech_brcm` tests (root-bus direct read, downstream index/data,
out-of-range/no-device, beyond-window fail-closed) + pcie_brcm tests (full
reset→SerDes→window→link sequence, bridge-reset-before-MISC ordering, ibar
encoding, fail-closed link-down + not-root-port, the wiring cap/mapper/
inert-window paths). Docs: `docs/src/drivers/bus.md`, the crate README.

**Landed — the in-kernel `DriverHost` serves both MMIO and DMA**
(host-provable): the in-kernel host the VL805 chain needs is complete.
`drvhost::HostConfig` gained an `mmio_mapper: Option<&dyn MmioMapper>`
seam and `DriverHost::mmio_mapper()` is implemented on the loaded-driver
view (alongside the existing `virtio_host()` DMA seam), so a loaded bus
driver maps its own register windows through the capability-gated
`KernelMmioMapper` *and* carves its DMA region through the per-driver
`KernelVirtioHost` — both fail closed at the kernel `map_mmio`/`alloc_dma`
gates (§5.4). `kernel/rustos-kernel/src/driver_host.rs`'s
`run_with_driver_host` assembles that host on a single boot frame
(`KernelMmioMapper` + `KernelVirtioFactory`) and lends it to a `body`
closure; every window and DMA pool is reclaimed when the closure returns
(§4). **No `lib/abi`/C-header change** (the seam is a trait-method
addition). Host-proven: drvhost `mmio_mapper_{default_none,some}_yields_*`
accessor tests + the `driver_host` `host_serves_both_mmio_and_dma_*` /
`driver_without_mmio_cap_is_refused_fail_closed` composition tests.

**Landed — outbound-window discovery into the hardware tree**
(host-provable): the `brcm,bcm2711-pcie` node now carries *both* address
windows the VL805 wiring needs, read from the device tree. Alongside the
inbound `dma-ranges` aperture (an `HwResource::dma`), the aarch64
`FdtDiscovery` now decodes the bridge's outbound `ranges` memory window
(`fdt::outbound_mmio_window`: the first memory-space entry's `phys.hi`
space code, the 64-bit PCIe base from `phys.mid`/`phys.lo`, the CPU base
and size from the parent cells) and emits it as a new
`HwResource::bus_window(cpu_base, size, pcie_base)`. That required
extending the hwtree ABI with `HwResourceKind::BusWindow` and a third
`xlate` (far-side/translated base) field on `HwResource` (WIRE_LEN
24→32; abi-v1 is unfrozen, §2.13) — a `BusWindow` is the general model
for a CPU↔bus address-translation window, distinct from a plain `Mmio`
register window so the controller `reg` is never conflated. C-header
regenerated. Host-proven: `fdt::outbound_*` decoder tests (memory
decode, I/O-space skip, absent/partial/out-of-range fail-closed), the
`platform::emits_the_pcie_bridge_with_its_outbound_window` emission test,
and the `HwResource::bus_window` round-trip in `lib/abi`.

**Landed — the USB-keyboard composition engine** (host-provable). The
inbound `dma-ranges` aperture is now emitted as
`HwResource::dma_translated(top, len, inbound_pcie_base)`
(`fdt::dma_ranges_aperture` captures the child PCI base in the resource's
translation field — no wire change, the `xlate` field already existed),
so `PcieWindows` is *fully* tree-derived. The whole chain is composed in
`kernel/rustos-kernel::usb_keyboard` — the image-assembly seam
(`Layer::Tooling`) is the one crate that may name the four driver crates
across strata (§17.4 / §8), so the composition lives there, like
`virtio_boot`; the engine is architecture-neutral (it consumes only the
`lib/abi` driver seams + the discovered `HwNode`) and un-gated, so it
compiles and host-tests on the CI host:

- `pcie_bringup_from_node(&HwNode) -> PcieBringup` reads the three
  resources (controller `Mmio`, inbound `Dma`, outbound `BusWindow`) off
  the discovered `brcm,bcm2711-pcie` node, fail-closed per missing
  resource (§2.9);
- `ChainHost` is a `DriverHost` view lending the bus driver the kernel's
  capability-gated MMIO mapper + per-driver DMA host (every map/alloc
  re-checked kernel-side, §5.4);
- `bring_up_keyboard(host, &PcieBringup, &dyn Delay)` runs the full chain
  — `pcie_brcm::wiring::open_discovered` (link train) → `mechanism_brcm`
  → `usb::wiring::open_discovered` → `UsbDevice::enumerate_first_connected`
  (new: scans root-hub ports 1..=max for the first connected device,
  fail-closed `NotFound` on an empty hub) → `BootKeyboard`;
- `QueueConsoleSink` feeds the produced bytes into the video console's
  `ConsoleInput` queue (`console_input`/`VIDEO_KEYBOARD`), short-pushing
  without spin (§2.1).

Host-proven: 8 `usb_keyboard` tests (window assembly + each fail-closed
missing resource + a non-zero inbound PCIe base, the sink delivers /
drops-overflow-without-spin, `ChainHost` reports caps/mapper/dma, the
chain fails closed without `CAP_MMIO_MAP`, and the chain reaches the
BCM2711 root-complex bring-up over a mapped window and fails closed
`DeviceFault` on the inert mock — the metal boundary), plus the usb
`enumerate_first_connected_*` tests.

**Landed — the aarch64 boot-path invocation** (host-proven up to the
metal boundary; the live bring-up is a metal-acceptance item, §0.4). The
production aarch64 boot path now starts the chain as an **in-kernel
keyboard service kthread**:

- `kernel/arch/aarch64::platform::pcie_bringup` resolves the
  `brcm,bcm2711-pcie` node's three windows (controller `reg`, inbound
  `dma-ranges` aperture, outbound `ranges`) with a single early-returning
  `scan_translated` walk, **pre-MMU-safe** (it reads only the matched
  node's own properties, like the console/GIC/video walks); a tree with no
  such node yields `None` (the `virt` shape, §18.4).
- `boot_aarch64` runs it pre-MMU, folds the controller-register and
  outbound-window gigapages into the identity **Device** mask
  (`identity_device_mask`) so both are identity-mapped Device memory before
  the MMU comes on, then stashes the `Copy` discovery for the spawn seam
  (`keyboard_service::record_discovery`) **after** the MMU is enabled — the
  seam's `SpinLock` `compare_exchange` is an atomic RMW that is
  UNPREDICTABLE on MMU-off memory (P6c-2), so the store is deferred past
  translation-enable while the windows themselves are read pre-MMU.
- `kernel/rustos-kernel::keyboard_service` supplies the concrete
  `DriverHost` halves: an `IdentityMmioMapper` (capability-gated; admits a
  window only inside the controller block or outbound window, returns a
  `phys == virt` `RegisterWindow` — no live page-table edit, since the boot
  path already mapped those gigapages, §5.4/§2.16) and a `FrameDmaHost`
  (capability-gated; carves the 16 KiB xHCI region with
  `FrameAllocator::alloc_order`, translates the frame to its device-visible
  address through the inbound viewport, and rejects anything outside the
  aperture, §5.4). A `GenericTimerDelay` over `kernel_arch::busy_delay_us`
  (`CNTPCT_EL0`) drives the link-training settle waits.
- The PID 1 spawn seam (`init_spawn`) calls
  `keyboard_service::spawn_if_present(ctx)` **before** `admit_init` drives
  the dispatch loop, so the service kthread is admitted onto the boot CPU's
  run queue and runs alongside PID 1. The body runs `bring_up_keyboard`
  once, then loops `usb_hid::pump_once` into the input-focus arbiter
  (`ArbiterConsoleSink`), yielding between polls (§2.1). A bring-up failure
  ends the service fail-closed (the video login parks with no keyboard,
  §2.9); with no discovered bridge (the `virt` shape) nothing is started.
- **Metal diagnostics (logging).** Because the bring-up is metal-only and
  was previously silent on failure, it logs one-shot, allocation-free
  events to the serial sink so a silent keyboard is diagnosable from a UART
  capture alone (§2.16/§19.4 — never on the poll loop): `boot_aarch64`
  logs the discovered `brcm,bcm2711-pcie` chipset windows (id `4100`),
  `spawn_if_present` logs the kthread admitted/skipped (id `4103`), and
  `bring_up_keyboard` logs each stage and the failing stage+`DriverError`
  (id `4101`), a one-shot post-link PCIe configuration scan listing every
  responding function (`function_count_hex` + per-function
  bdf/vendor/device/class, id `4104`), plus the enumerated device's
  vid/pid/slot (id `4102`). See `docs/src/platform/aarch64.md`.
- **Downstream config forwarding + single-device config gate (fixed).**
  The VL805 on bus 1 was invisible because the BCM2711 ships the root
  port's type-1 bridge bus-number register (`PCI_PRIMARY_BUS`, config
  offset `0x18`, exposed at the controller register block's offset 0) at 0,
  forwarding no configuration to its secondary bus. `BrcmPcieRc::bring_up`
  programs it (`program_bridge_bus_numbers`, primary 0 / secondary
  `RC_SECONDARY_BUS` 1) when it presents the RC as a PCI-PCI bridge.
  Enabling forwarding then exposed a boot **wedge**: the bus walk is a flat
  256-bus scan, and a config read to a non-existent forwarded target (any
  device but `01:00.0`, or any bus beyond the directly-attached one) emits
  a TLP nothing answers — `CFG_READ_UR_MODE` only master-aborts requests
  the RC itself refuses, so a *forwarded* TLP's completion timeout becomes a
  CPU external abort that hangs the boot CPU (capture stops right after
  `4101 link trained`). Fixed in the windowed accessor: the BCM2711 root
  port is a single-device link, so `mechanism_brcm(window, secondary_bus)`
  forwards a transaction **only** to `device 0` on the secondary bus and
  resolves every other downstream target to the `0xFFFF_FFFF` sentinel
  without touching the controller (it never forwards to an absent target); the
  bridge subordinate is kept equal to the secondary (no on-board switch,
  §2.3). Host-proven by `bring_up_names_the_downstream_bus_so_config_is_forwarded`
  and `mech_brcm::phantom_downstream_targets_are_no_device_without_an_index_write`.
  The metal `4104` capture then listed **two** functions (the bridge plus
  the VL805 `1106:3483` class `0c03`), confirming discovery is complete.
- **xHCI DMA-aperture bound — CPU-vs-PCIe address space (fixed).** With the
  VL805 discovered, the `4101` xHCI controller bring-up failed
  `err=out_of_range`: the device-shared DMA carve was bounded in the wrong
  address space. `DmaSlab::phys()` is a *device-visible* (PCIe-space)
  address, but `keyboard_service::FrameDmaHost` and
  `rustos_drv_bus_usb::wiring::open_discovered` compared it against the
  *CPU-physical* inbound-aperture top (`0x2_0000_0000`). The Pi 4 inbound
  viewport maps PCIe `[0x4_0000_0000, 0x6_0000_0000)` onto RAM
  `[0, 0x2_0000_0000)`, so every frame's device address (≈ `0x4_xxxx`)
  exceeded the CPU top and the carve was refused before any hardware was
  touched. Fixed by bounding each side in its own space: `FrameDmaHost` now
  checks the frame's CPU-physical span against the CPU window top and
  translates afterwards, and `bring_up_keyboard` passes `open_discovered`
  the device-visible top (`inbound_pcie_base + inbound_size`, checked). The
  redundant, address-space-ambiguous `PcieBringup.dma_aperture_top` field
  (derivable from `windows`) was removed (§2.2/§2.14). Host-proven by
  `keyboard_service::dma_host_admits_a_low_frame_through_a_high_pcie_viewport`.
- **xHCI register BAR mapping — bridge-aware translation + unassigned-BAR
  assignment (fixed).** `keyboard_service::IdentityMmioMapper` is
  bridge-aware: it applies the outbound `ranges` translation
  (`outbound_cpu_base + (bus − outbound_pcie_base)`) to reach the
  identity-mapped CPU address (the generic PCI walk only knows bus addresses;
  resolving them is the host bridge's job), the controller
  regs block stays CPU-physical/identity and is resolved first, and a request
  only partially overlapping the numerically-overlapping Pi 4 regs island
  (`0xfd50_0000`) is refused fail-closed (§5.4). `IdentityMmioMapper::new`
  takes `outbound_pcie_base`; `with_diag(&SERIAL_SINK)` logs each map decision
  one-shot (`EventId(4105)`, off the poll path) so a metal capture shows the
  refused base. That capture localised the real cause: the VL805's BAR0
  address bits read **zero** — the BAR is sized/typed but *unassigned*
  (firmware programs BARs, but resetting and re-enumerating the root complex
  leaves the downstream function unassigned), so mapping it targets address 0.
  Assigning resources from the bridge's outbound window is the PCI core's job;
  added `PciBus::assign_bar(bdf, bar_index, window_base,
  window_size)` (implemented on `Pci<C>`): it probes BAR size/type and, when
  the address bits are zero, writes the lowest size-aligned PCIe-bus address
  in the window (both dwords for a 64-bit BAR, control bits preserved); an
  already-based BAR is left untouched (no-op under QEMU/firmware-assigned).
  `usb::wiring::open_discovered` takes the outbound window and calls
  `assign_bar` before `map_bar_window`. Host-proven by
  `pci::assign_bar_places_an_unassigned_64bit_bar_in_the_window` (+ idempotent
  / oversize / I/O / absent cases), `usb::open_discovered_enables_mastering_
  and_reaches_the_controller` (asserts the window reaches `assign_bar`), and
  `keyboard_service::mapper_translates_a_bar_through_the_outbound_viewport`.
- **VL805 firmware fallback (current).** `open_controller` follows the safe order: configure the PCIe bridge and assign/map BAR0 first, read the
  VL805 firmware version at config `0x50`, skip any reload when it is already
  non-zero, and issue exactly one `NOTIFY_XHCI_RESET` only when the bounded
  version wait stays `0`. The mailbox uses the shared `lib/vcmailbox`
  protocol over the boot-discovered VideoCore doorbell and a cache-maintained
  static property buffer; `4108` records the fallback outcome and `4113` the
  diagnostic response value. Host-proven by
  `open_controller_reloads_firmware_when_version_stays_zero`,
  `firmware_reload_is_skipped_when_version_is_already_loaded`, and the
  `lib/vcmailbox` encode/decode/coherency tests. Live keyboard enumeration is
  the remaining on-metal acceptance item (QEMU models no Pi PCIe/USB, §0.4).
- **BAR still `dead_dead` after the reload = bridge memory window never
  forwarded; fixed by programming it during root-complex bring-up.** A
  later metal capture showed the reorder + readiness poll necessary but not
  sufficient: `4108 Reloaded` and `4109 … ready_hex=0` with the capability
  header *still* uniform `dead_dead` after the full budget. The cause was
  not the reload but a missing bring-up step: a PCI-PCI bridge forwards a
  *memory* transaction downstream only when the address lies inside its
  **Memory Base/Limit** window (type-1 config offset `0x20`), and the
  BCM2711 ships it empty (base `0`, limit `0`). The bring-up had programmed
  the bridge *bus-number* register (so *config* reads reached the VL805 —
  the `4104` scan saw `1106:3483`) and the controller's CPU→PCIe outbound
  *translation* (`MEM_WIN0`), but never the bridge memory window, so the
  root port master-aborted every CPU access to the VL805's BAR. The fix is
  `BrcmPcieRc::program_bridge_mem_window` (run right after the bus-number
  programming): it sets the bridge Memory Base/Limit to cover the discovered
  outbound PCIe range (`[outbound_pcie_base, +outbound_size)`), the range
  BARs are assigned within, as a full PCI enumerator does (which the
  windowed `mech_brcm` accessor does not perform). The register encodes only
  address bits `[31:20]` and the non-prefetchable window decodes below 4 GiB,
  so a window reaching the 4 GiB line fails closed (§5.4); the BCM2711's
  outbound base `0xc000_0000` is well below it. Host-proven:
  `bring_up_opens_the_bridge_memory_window_so_bar_reads_are_forwarded`
  asserts the window covers `0xc000_0000..0x1_0000_0000`. Metal acceptance
  item: a healthy capture should now read a real `CAPLENGTH` at `4107`/`4109
  ready_hex=1`; a `4109 ready_hex=0` with `dead_dead` after this fix would
  localise the fault past the bridge window to the reload step itself
  (mailbox sequence / `dev_addr` / board firmware). QEMU models no Pi PCIe
  (§0.4).
- **`4110` configuration read-back — measuring which write stuck after the
  bridge-window fix.** A capture *after* the bridge-window fix still showed
  `4108 Reloaded`, `4109 ready_hex=0` and `4107` uniform `dead_dead`. At that
  point the whole controller/bridge programming chain is present in code
  (bridge bus numbers, bridge Memory Base/Limit, CPU→PCIe outbound
  translation, VL805 BAR assignment, VL805 command-register memory-space +
  bus-master), yet the mapped BAR aborts while *config* reads still succeed.
  Rather than guess again (§15.7), `open_controller` now reads each
  programmed register *back* (one-shot `EventId(4110)`, after BAR assign +
  command enable, before `Xhci::open`): bridge `0x18`/`0x20`/`0x04` and VL805
  `0x04`/`0x10`/`0x14`. Served by a new read-only `PciBus::read_config(bdf,
  offset)` (lib/abi, implemented on `Pci<C>`, reaches both the bus-0 bridge
  and the VL805 via `mech_brcm`); fail-closed (a faulting read renders the
  all-ones sentinel, never propagated, §2.9), one-shot (never on the poll
  path, §2.16/§19.4). Host-proven: `read_config_returns_the_dword_at_the_byte_offset`
  (abi), `config_readback_dumps_each_register_once` +
  `config_readback_renders_a_sentinel_for_a_faulting_read` (usb_keyboard).
  QEMU models no Pi PCIe/USB (§0.4).
- **Enable the root-port bridge Command register *after* the link is up
  (done; landed on metal but not the fix).** `BrcmPcieRc::program_bridge_command` sets Memory Space +
  Bus Master in the root port's Command register (config `0x04`, the standard
  PCI-PCI bridge enable). The `4110` capture showed it read back `0x0000`
  while the adjacent bus-number (`0x18`) and Memory Base/Limit (`0x20`) writes
  — same direct bus-0 path — stuck, so the offset is right and the difference
  is *timing*: the integrated RC latches Memory Space Enable only against a
  **live link**, and the earlier bring-up wrote it during the config phase
  with `PERST#` still asserted. A working boot chain enables the root port
  only after the link trains. Fix: `bring_up` now calls `program_bridge_command`
  **last**, after `train_link` + the fail-closed `link_up()` confirmation, so
  MEM-space/bus-master latch against the trained link. This is the unifying
  explanation for the triple symptom — `VideoCore` reaches the VL805 over the
  same configured bus to load firmware ("VideoCore expects from us a
  configured PCI bus"), so an un-latched bridge command master-aborts *both*
  our BAR reads *and* the firmware-load writes, leaving `dead_dead`,
  `fw_version=0`, and `response=0` together. Host-proven:
  `bring_up_enables_memory_space_and_bus_master_on_the_bridge` (asserts the
  command write follows the final `PERST#`-deassert). **Metal outcome:** the
  later capture confirmed `bridge_command_status_hex` now reads back `0x6`
  (the latch fix works) — but the VL805 BAR is *still* `dead_dead` and
  `vl805_fw_version_hex` *still* `0` both before and after the reload, so the
  bridge command was a real defect but not the firmware-load fix. QEMU models
  no Pi PCIe/USB (§0.4).
- **Gate readiness on the VL805 firmware version, not the aborting BAR
  (done; the gate is the correct signal — firmware-load is the metal-only
  residual).** `open_controller` gates readiness on `wait_for_firmware_loaded`:
  it polls the VL805 **XHCI MCU firmware version** (config `0x50`, the
  *working* vendor firmware-version register) for a non-zero
  build id, bounded by `FW_LOADED_BUDGET_US` (~2 s wall time) — strictly more
  correct than the old gate, which polled the master-aborting BAR. The
  `NOTIFY_XHCI_RESET` reload is demoted to fire **only** if the version stays
  `0` (a redundant reload of an already-(re)loading VL805 can kill it,
  raspberrypi/firmware #1380), then the BAR caps wait (`4109`) runs once the
  firmware is loaded. Logged one-shot as `EventId(4118)`
  (`polls_hex`/`fw_version_hex`/`ready_hex`). Host-proven:
  `open_controller_reloads_the_firmware_as_a_fallback_when_version_stays_zero`
  (the reload fires + two `4118` records when `0x50` stays `0`) and
  `open_controller_skips_the_firmware_reload_when_mapping_fails`.
  **Decisive metal + known-good datapoint (this confirms the gate and
  closes the host-side investigation):** the `4118` capture shows
  `fw_version_hex=0` through the full ~2 s budget, a no-op `NOTIFY_XHCI_RESET`,
  and a second `4118` still `0`. On the *same* board a known-good capture
  reads `0x50 = 0x000138c0` (a real firmware build id
  `VideoCore` writes on load), `10.l = 0xc000000c` (BAR0 based at `0xc000_0000`,
  **prefetchable**), `04.w = 0x0546` (mem-space + bus-master). So `0x50` *is*
  the genuine firmware-version register on this board, and our `0` /
  non-prefetchable `BAR0=0xc0000004` / `dead_dead` are all consistent symptoms
  of a genuinely firmware-less VL805 — i.e. the `4118` gate observes exactly the
  right thing and is **not** a red herring. Every host-verifiable PCI element
  (outbound window decode, bridge command/bus-numbers/mem-window, BAR address,
  and the `NOTIFY_XHCI_RESET` message: tag `0x30058`, dev_addr `0x10_0000`) is
  proven correct. The sole residual is that our
  `NOTIFY_XHCI_RESET` is *honoured* (tag response bit set) yet a no-op
  (firmware version never advances), where the same flow elsewhere loads the
  blob — a `VideoCore`/board-firmware-state matter that produces no further
  signal on the (now-proven-correct) PCI side and cannot be reproduced or
  verified without the hardware (QEMU models no Pi PCIe/USB, §0.4).
- **Inbound-window lead (raspberrypi/firmware #1617, then pftf/RPi4 #1495)
  — IN PROGRESS (metal capture pending).** #1617 suggested `VideoCore`
  loads the VL805 firmware over PCIe **through an inbound DMA window**. The
  `4119` capture (`BrcmPcieRc::inbound_window_readback`, post-program) reads
  `rc_bar2_hi=0x4`, size `0x12` (8 GiB), `RC_BAR1`/`RC_BAR3` disabled —
  byte-identical to the known-good **runtime** window
  `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`. But #1495 shows the
  `NOTIFY_XHCI_RESET` load *assumes* a particular `RC_BAR2` state instead of
  reading it back, and the blob is **not** loaded at runtime, so
  matching its runtime window does not prove the window matches what
  `VideoCore` assumes at the load moment. Two changes pursue this (host-proven;
  decisive datapoint is metal):
  - `BrcmPcieRc::entry_inbound_window` captures `RC_BAR2`/`RC_BAR1`/`RC_BAR3`
    **as `start4.elf` left them**, before bring-up touches them, logged
    one-shot as `EventId(4120)` (`log_entry_inbound_window`); comparing `4120`
    vs the post-program `4119` shows whether our reprogramming diverged. Tests:
    `entry_inbound_window_reports_the_state_before_bring_up_programs_it` /
    `..._logs_one_4120_record`.
  - `bring_up` now **preserves a firmware-configured `RC_BAR2`** (a non-zero
    size field) rather than overwriting it, honouring VideoCore's assumed
    state; it only programs the inbound window from discovery when the firmware
    left it unconfigured. Test:
    `bring_up_preserves_a_firmware_configured_inbound_window`.
  - The mailbox `Timeout` is now localised: `vcmailbox::ExchangeStats`
    (`MmioMailbox::last_exchange_stats`) records the timeout stage (post-room
    vs response), posted word, poll counts and last status, logged as
    `EventId(4121)` (`keyboard_service::log_mailbox_exchange`). Test:
    `mmio_exchange_stats_localise_the_timeout_stage`.
  - A **runtime mailbox liveness probe** (`vcmailbox::query_firmware_revision`,
    `GET_FIRMWARE_REVISION` tag `0x1` — a state-free read) is issued over the
    same transport immediately **before** the `NOTIFY_XHCI_RESET` reload and
    logged as `EventId(4122)` (`keyboard_service::log_mailbox_probe`,
    `probe_outcome` + `firmware_revision_hex` + the shared `ExchangeStats`
    fields). It separates a broken post-MMU mailbox path (`probe_outcome=timeout`)
    from `VideoCore` dropping only the xHCI tag (`probe_outcome=ok` with a
    non-zero revision, then a `4121` reload timeout). Tests:
    `firmware_revision_query_lays_out_the_get_tag`,
    `firmware_revision_round_trips_through_a_healthy_firmware`,
    `firmware_revision_decode_fails_closed`.
  - **Mailbox response-length handling audited and confirmed correct**
    (against the VideoCore property protocol as documented in the
    raspberrypi.stackexchange.com #133040 answer). The classic bug — telling
    VideoCore a `0`-byte value buffer so it cannot reply — does not occur:
    `push_tag` sizes every tag's value-buffer length word to
    `max(request, response)` words (`ALLOCATE`=8 B, `GET_PITCH` /
    `GET_FIRMWARE_REVISION` / `NOTIFY_XHCI_RESET`=4 B), and `find_tag` reads
    the per-tag response data bounded by that buffer. Because we never
    under-provision, the protocol's "response length may exceed buffer length,
    real data = `min(buffer, response)`" truncation case cannot arise for our
    fixed-layout tags, so `find_tag` fails such a reply closed rather than
    clamping (`AGENTS.md` §5.4). Pinned by `encode_lays_out_header_tags_and_end_marker`,
    `xhci_reset_lays_out_the_dev_addr_tag`,
    `firmware_revision_query_lays_out_the_get_tag` and
    `decode_rejects_short_and_oversized_tag_responses`. The metal
    `4122 probe_outcome=ok` (revision read over the same buffer) with
    `4121 timeout_stage=response` is therefore VideoCore dropping the xHCI tag
    at the firmware handoff, **not** a response-length/buffer-sizing fault.
  - **Firmware-version register no longer gates the bring-up — probe the
    authoritative BAR capability block instead (latest experiment;
    `AGENTS.md` §2.9 / §15.7).** The config-space `0x50` register is a VL805
    *vendor* convenience, not the xHCI controller's readiness signal; the
    controller's own capability block (`CAPLENGTH`/`HCIVERSION` on the BAR,
    xHCI 1.2 §5.3) is. The metal capture (`4118`/`4121`/`4122`) shows `0x50`
    staying `0` and `VideoCore` dropping the working mailbox's
    `NOTIFY_XHCI_RESET` tag, yet on a board whose boot firmware left the
    controller's MCU firmware resident the capability block can be live
    regardless. So `open_controller` now treats the firmware-version wait +
    one-shot `NOTIFY_XHCI_RESET` reload as **best-effort/diagnostic**: it
    records the decision as `EventId(4123)` (`log_firmware_gate`,
    `firmware_loaded_hex`) and **always proceeds** to `wait_for_caps_ready`
    (`4109`) + `Xhci::open` — the real fail-closed gate — rather than aborting
    at `DeviceFault` before the BAR was ever probed (the previous behaviour
    never logged `4109`/`4107`/`4114`/`4106` on the failing metal path). Tests:
    `open_controller_probes_the_bar_when_reload_does_not_make_version_loaded`,
    `open_controller_probes_the_bar_when_firmware_reload_fails`,
    `open_controller_proceeds_after_reload_makes_version_loaded`. **Decisive
    metal datapoint:** the now-reachable `4109 ready_hex=1` (a live
    `CAPLENGTH`/`HCIVERSION` at `4107`) with the keyboard enumerating (`4102`)
    proves the controller's firmware is resident and the in-tree path is
    complete regardless of `0x50`; `4109 ready_hex=0` with `dead_dead` caps
    confirms the controller genuinely never decodes, pinning the residual on
    the boot-firmware handoff below. Metal-only (QEMU models no Pi PCIe/USB,
    §0.4).
  - **`4124` post-reload PCIe error snapshot — done: no master abort, VL805
    reachable.** The read-only `log_bridge_error_status` (`4124`) logged
    immediately after the reload reads the root-port command/status (`0x04`),
    the root-port **Secondary Status** (config `0x1C`, bits `[31:16]`), and
    the VL805 command/status (`0x04`). The metal capture read
    `bridge_secondary_status=0` (no Received Master/Target Abort) with the
    bridge + VL805 commands enabled (`0x...0006`/`0x...0146`), so
    `VideoCore`'s firmware-load is **not** master-aborting on the bridge — the
    downstream VL805 path is reachable. This **disproves** the bus-reach
    hypothesis (the former "option B": realigning `pcie_brcm` inbound
    `dma-ranges`/`RC_BAR2` with the BCM2711 PCIe bring-up sequence is no longer indicated —
    the BAR capability block also reads live at `4107`/`4109`, confirming the
    PCI path works end-to-end). The dropped `4121 timeout_stage=response`
    therefore points at `VideoCore`'s loader, not the bus. Host-proven by the
    `4124` assertion in
    `open_controller_probes_the_bar_when_reload_does_not_make_version_loaded`.
  - **Reload response-wait too short — done: the reload now completes.** The
    reload formerly busy-polled `DEFAULT_POLL_BUDGET` (1,000,000) iterations,
    which the metal capture showed completing in only ≈400 ms (the
    `4122`→`4121` gap) — well under the **full second** the vendor bring-up allows the same
    property call. The reload mailbox now uses a dedicated `FIRMWARE_RELOAD_POLL_BUDGET`
    (`10 × DEFAULT_POLL_BUDGET`, ≈4 s of metal wall time, above that 1 s),
    and `reload()` measures the reload's real `CNTPCT_EL0` wall time (shared
    pure `keyboard_service::counter_elapsed_us`) into the `4121`
    `wait_elapsed_us_hex` field (with `poll_budget_hex`). The follow-up metal
    capture **confirmed** the fix: `4121 timeout_stage=none`, `last_status=1`,
    `wait_elapsed_us≈0x16266` (≈90 ms), and `4108` now logs *reloaded*
    (success) rather than the old timeout — the prior give-up was premature.
    Host-proven by `counter_elapsed_us_converts_a_tick_span_at_the_counter_rate`
    and `counter_elapsed_us_fails_closed_on_a_zero_or_reordered_sample`.
  - **Inbound SCB window unsized — `MISC_CTRL_SCB0_SIZE`: done, metal-confirmed.**
    Comparing our `pcie_brcm` `bring_up` against the known-working BCM2711
    PCIe bring-up sequence found the one
    concrete divergence: both program `MISC_CTRL.SCB0_SIZE =
    ilog2(round_pow2(region)) - 15` (bits `[31:27]`, mask `0xf800_0000`) to
    size the inbound SCB (PCIe→system-memory) decode window to the DMA region,
    **unconditionally** on the BCM2711, while our `bring_up` left `SCB0_SIZE`
    at its reset default. An undersized inbound decoder silently drops a
    PCIe→memory DMA past that small window while config reads, enumeration, BAR
    assignment and the outbound path all succeed (the `4124` snapshot showed
    **no** master-abort), so `VideoCore`'s `NOTIFY_XHCI_RESET` firmware-load
    completed yet never landed the blob. `pcie_brcm` now programs `SCB0_SIZE`
    (`encode_scb_size`, fail-closed `0` outside 64 KiB‥64 GiB; `0x11` for the
    Pi's 4 GiB viewport) and the inbound read-back (`4119`/`4120`) carries a
    `misc_ctrl_hex` field. **The metal capture confirmed the fix decisively:**
    `4118 fw_version=0x138c0 ready=1`, `4108` *reloaded*, `4123
    firmware_loaded=1`, and `Xhci::open` brought the controller fully online
    (`4101 … enumerating root hub`, `4106 max_ports=5`). The VL805 firmware
    handoff is solved — the long firmware-never-loads investigation is closed.
    Host-proven by `encode_scb_size_sizes_the_inbound_scb_window_to_the_region`,
    `bring_up_trains_the_link_and_programs_the_windows` (SCB0 assertion) and
    `inbound_window_readback_reports_the_programmed_viewport`.
  - **Root-hub port power (`PORTSC.PP`): done, metal-confirmed.**
    With the controller online, the residual moved to root-hub enumeration:
    `4101 no usb device enumerated on the root hub err=device_fault`. The scan
    (`UsbDevice::enumerate_first_connected`) read each port's Current Connect
    Status **once**, immediately after Run, with no Port Power asserted and no
    connect debounce — but the Host Controller Reset in `Xhci::open` clears
    every `PORTSC`, and the VL805 is port-power-controlled (`HCCPARAMS1`
    PPC = 1), so a powered-off port reports disconnected regardless of what is
    attached (xHCI 1.2 §4.19.1.1). The scan now asserts `PORTSC.PP`
    (`Xhci::set_port_power`, masking the write-1-to-clear bits) on every
    reported port, then debounce-polls `1..=max_ports` (bounded by the engine
    budget, fail-closed `NotFound` on a genuinely empty hub) for the first port
    to report a device; `EventId(4125)` logs every root port's post-power
    `PORTSC` (raw + decoded `ccs`/`pp`/`ped`/`speed`) on the failure path.
    **The metal capture confirmed the fix:** `4125` now reads port 1
    `ccs=1 pp=1 ped=1 speed=3` (a connected, enabled, high-speed device) with
    the other four ports powered but empty — so power asserts correctly and the
    device is present. Host-proven by `drivers/bus/usb`
    `set_port_power_asserts_pp_and_rejects_a_bad_port`,
    `enumerate_first_connected_powers_every_root_port` and
    `enumerate_first_connected_connects_a_port_only_after_power`.
  - **Enumeration fault localisation (`EnumStage` / `4126`): done, the
    localiser that pinned the next root cause.** With a device on port 1 the
    `device_fault` was *inside* `UsbDevice::enumerate_hid`; the single coarse
    `DriverError::DeviceFault` could not say where. The driver records a
    breadcrumb — `enum_stage()` (an `EnumStage` discriminant set as each step
    runs) and `last_completion_code()` (the raw xHCI completion code of the last
    event that step observed, reset to `0` = none/timeout at the start of each
    command/control transfer, set undecoded via `Trb::completion_code_raw`) —
    logged one-shot on the failure path as `EventId(4126)`
    (`stage_hex` + `completion_hex`) before the `4125` port dump. Host-proven by
    `drivers/bus/usb` `enumerate_hid_records_the_configured_stage_on_success`
    and `enumerate_hid_stage_breadcrumb_localises_a_class_stall`.
  - **Non-coherent DMA cache maintenance — done; necessary but not
    sufficient (`AGENTS.md` §4 / §15.7).** The `4126` metal capture read
    `stage_hex=2` (Enable Slot)
    `completion_hex=0`: the *first* command issued to the online controller
    never produced a Command Completion event, although the capability block
    reads live over MMIO and a device is attached — the controller is not
    consuming the command ring at all, the signature of a **cache coherency**
    gap. The BCM2711 PCIe root complex is **not** I/O-coherent (the very reason
    the VideoCore mailbox and the HVS framebuffer already clean/invalidate on
    this platform), yet the xHCI device-shared DMA region is plain cacheable,
    identity-mapped RAM and its `DmaRegion for DmaSlab` read/write did **no**
    maintenance: the command-ring TRB the CPU wrote stayed in a dirty cache
    line the controller never saw (stale memory → no command → no completion),
    and symmetrically the CPU would read a stale event ring. The fix gives
    `DmaSlab` an optional `SlabCoherencyFn` (`with_coherency`) and a
    `sync_range(offset, len)` that cleans **and** invalidates the touched range
    to the point of coherency; the USB driver's `DmaRegion` impl invalidates
    **before** every read and cleans **after** every write, and
    `keyboard_service::FrameDmaHost` wires the aarch64
    `clean_invalidate_dcache_range` (`dc civac` + `dsb`) into every minted slab
    (a slab without a shim — coherent interconnect / host test — skips it).
    Host-proven by
    `dma_slab_sync_range_brackets_only_in_bounds_ranges_through_the_hook`
    (lib/virtio) and
    `dma_slab_region_brackets_writes_and_reads_with_cache_maintenance`
    (drivers/bus/usb). A rebuilt image carrying the maintenance still captured
    `4126 stage_hex=2 completion_hex=0`, so it was necessary but not the whole
    cause — the controller had a second reason not to consume the command ring
    (the scratchpad lever below).
  - **Scratchpad buffers — done; `AGENTS.md` §4 / §15.7.** The
    residual `stage=2 completion=0` was the missing xHCI **scratchpad buffers**.
    `HCSPARAMS2` Max Scratchpad Buffers names page-sized buffers software must
    reserve and point `DCBAA[0]` at before the controller can run any command
    (xHCI §4.20); the VL805 datasheet reports `HCSPARAMS2 = 0xFC00_0031` — **31**
    buffers (`SPR = 1`) — but the driver read neither `HCSPARAMS2` nor `PAGESIZE`
    and allocated none, so `DCBAA[0]` stayed zero, the controller had nowhere to
    save state, and it executed no command despite accepting Run/Stop and
    reporting a live capability block. `Xhci::open` now reads `HCSPARAMS2`/
    `PAGESIZE` (`max_scratchpad_buffers()` / `page_size()`, surfaced as `4106`
    `max_scratchpad_hex`); `device::Layout` reserves a page-aligned scratchpad
    pointer array plus that many page-aligned buffer pages (fail-closed if the
    carve cannot hold them or a scratchpad-needing controller reports no page
    size / an unaligned base); `UsbDevice::start` fills the array with each
    buffer's device-visible base and points `DCBAA[0]` at it; and the carve
    (`wiring::XHCI_DMA_BYTES`) grew 16 KiB → **256 KiB** to hold 31 × 4 KiB
    pages plus the rings/contexts. Host-proven by `drivers/bus/usb`
    `start_reserves_scratchpad_and_programs_dcbaa0` (a mock that, like the VL805,
    withholds every command completion until `DCBAA[0]` is programmed —
    enumeration then runs end to end),
    `start_stalls_without_scratchpad_on_a_controller_that_needs_it`, and the
    `hcsparams2_decodes_the_vl805_scratchpad_count` /
    `pagesize_decodes_the_lowest_supported_page` decode tests. **Decisive metal
    datapoint:** `4106 max_scratchpad_hex=0x1f` confirms the count is read, and
    the keyboard enumerating (`4102`) — or `4126` advancing past `stage_hex=2` —
    confirms the missing scratchpad was the blocker; a still-stuck `stage_hex=2
    completion_hex=0` means the command ring is still not consumed for a further
    reason. Metal-only beyond the host tests (QEMU models no Pi PCIe/USB, §0.4).
    **Metal-confirmed:** the capture read `4106 max_scratchpad_hex=0x1f` and
    `4126` advanced to `stage_hex=8 completion_hex=6` — the command ring runs and
    the device addresses, reads its descriptors, and configures.
  - **`SET_PROTOCOL(boot)` STALL tolerated — done; `AGENTS.md`
    §2.9 / §15.7.** With the scratchpad reserved, enumeration reached the
    last step and stopped at `4126 stage_hex=8 completion_hex=6`: the HID
    `SET_PROTOCOL(boot)` class request (`EnumStage::SetProtocol`) answered
    **STALL** (code `6`). `SET_PROTOCOL` is mandatory only for boot-subclass
    devices (HID 1.11 §7.2.6); a device that does not implement it STALLs,
    and a STALL on the default control endpoint is a *protocol* stall that
    auto-clears on the next SETUP (USB 2.0 §8.5.3.4), so the device stays
    usable in its default protocol (a non-implementing device ignores a
    stalled `SET_PROTOCOL`). The driver treated any non-Success control
    completion as `device_fault`, aborting an otherwise enumerable keyboard.
    `enumerate_hid` now issues `SET_PROTOCOL(boot)` through `control_optional`,
    which absorbs a STALL (raw code preserved in `last_completion`), primes
    the interrupt-IN ring, and reaches `EnumStage::Configured`; every other
    completion still fails closed. It is the last EP0 transfer and EP0 is not
    reused, so a halted control endpoint after the STALL is immaterial.
    Host-proven by `drivers/bus/usb` `enumerate_hid_tolerates_a_stalled_set_protocol`
    and `enumerate_hid_fails_closed_on_a_non_stall_class_fault`.
    **Metal-confirmed:** the capture logs `4102 vendor=2109 product=3431` —
    enumeration runs end to end, so the tolerated STALL was the last
    *enumeration* blocker. What enumerates, however, is the onboard hub, not
    the keyboard (the hub-topology lever below).
  - **Hub topology.** The `4102` device is `2109:3431` — the Pi 4B's
    **onboard VIA Labs USB hub** between the VL805 root hub and the four USB-A
    ports. The keyboard is plugged into a USB-A port, so it enumerates
    *downstream* of that hub, not on a root-hub port; the bring-up enumerated
    and configured the hub itself but a hub emits no HID reports, so login
    still sees no keystrokes. The enumerated device's `bDeviceClass` is `0x09`
    (`DeviceDescriptor::is_hub`). When the device is a hub the bring-up reads
    its `bNbrPorts` (class `GET_DESCRIPTOR(hub)`), asserts Port Power on every
    downstream port (class `SET_FEATURE(PORT_POWER)`), waits
    `HUB_POWER_ON_GOOD_US` (~100 ms), and logs each downstream port's class
    `GET_STATUS` as `EventId(4127)` (`UsbDevice::hub_num_ports` /
    `power_hub_port` / `hub_port_status` over the hub's already-addressed EP0).
    The `4127` record with `connected_hex=1` pins which downstream port the
    keyboard is on and its `speed_hex`.
  - **EP0 halted by `SET_PROTOCOL` on the hub — done; `AGENTS.md` §15.7.**
    The metal capture read `4101 reading the hub descriptor failed
    err=device_fault`: the class `GET_DESCRIPTOR(hub)` faulted though the
    device/config-descriptor reads on the same EP0 had succeeded. Root cause:
    `enumerate_hid` issued the HID `SET_PROTOCOL(boot)` to *every* device,
    including the hub; a hub is not a HID device so it STALLs that request, and
    an xHCI STALL **halts** the control endpoint (xHCI §4.10.2.4) until reset.
    `control_optional` tolerated the STALL on the (now-broken) assumption that
    it is the last EP0 transfer of enumeration — but a hub reuses EP0 for the
    hub-descriptor read, which then ran on a halted endpoint. Fixed by issuing
    `SET_PROTOCOL(boot)` **only** to a HID interface (`InterfaceInfo::is_hid`,
    `bInterfaceClass == 0x03`); a hub (interface class `0x09`) never receives
    it, so its EP0 stays usable. A keyboard plugged directly into the root hub
    is unaffected (its HID interface still gets it, still last). The `4127`
    hub-descriptor-read failure log now also carries `completion_hex` (raw xHCI
    completion code). Host-proven by `drivers/bus/usb`
    `enumerate_hid_flags_a_hub_via_the_device_class`,
    `enumerating_a_hub_leaves_ep0_usable_for_the_hub_descriptor` (the mock
    STALLs the hub's `SET_PROTOCOL` and models the EP0 halt — fails before the
    gate, passes after), `hub_discovery_finds_the_downstream_device`,
    `hub_port_reads_disconnected_until_powered`, and
    `hub_num_ports_fails_closed_on_a_forged_descriptor`.
  - **Per-port `GET_STATUS` faults — current lever; `AGENTS.md` §15.7.** With
    the EP0 fix the hub-descriptor read succeeds (metal `4127 num_ports=4`) and
    Port Power is asserted on every downstream port, but the capture then read
    every port's `wstatus_hex=0xffff` (the all-ones sentinel) with
    `completion_hex=0` — each per-port class `GET_STATUS` (USB 2.0 §11.24.2.7)
    faulted while `GET_DESCRIPTOR(hub)` and `SET_FEATURE(PORT_POWER)` on the
    same EP0 succeeded. The sentinel-decoded `connected=1 speed=2` are
    artifacts, not real reads, so the downstream port holding the keyboard is
    still unknown.
  - **`completion_hex=0` was a diagnostic gap, now fixed (done).** The four
    `4127` records are spaced at the same ~250 ms serial cadence as the
    non-faulting `4125` lines, so each `GET_STATUS` failed *fast* (not after
    the million-iteration budget) — an event almost certainly arrived.
    `UsbDevice::control`/`command` recorded `last_completion` only **after**
    `await_event_for` returned `Ok`, but `await_event_for` returns `Err` before
    that on an unexpected TRB address *or* a completion code outside the
    modelled set {1,2,3,4,5,6,13} (its fail-closed `completion_code()` decode),
    leaving the `0` "no event" sentinel — so a real-but-rejected code was
    mislabelled as a timeout. `await_event_for` now records
    `last_completion = completion_code_raw()` the instant it observes any
    command/transfer event (before the address match and the decode), so
    `completion_hex` is truthful: a genuine `0` now means no event at all, a
    non-zero value names what the hub answered (including a reserved /
    controller-specific code). The now-redundant post-`await` assignments in
    `control`/`command` were dropped (§2.2/§2.14). Host-proven by
    `faulting_hub_port_status_records_the_completion_code` (STALL `6`) and the
    new `faulting_hub_port_status_records_an_undecodable_completion_code` (the
    unmodelled xHCI code `7`: fails closed on the decode yet
    `last_completion_code()` now retains `7` — read `0` before the fix).
  - **Latest capture + reject localisation — current lever; `AGENTS.md`
    §15.7.** With `completion_hex` truthful the metal capture named it: ports
    1–2 read `completion_hex=0x0d` (xHCI ShortPacket, the IN data stage), ports
    3–4 `completion_hex=0`, **all** still failing closed (`wstatus=0xffff`).
    Every *other* EP0 control transfer (device/config/hub descriptors,
    `SET_ADDRESS`/`SET_CONFIGURATION`, `SET_FEATURE(PORT_POWER)`) succeeds on
    the same EP0, so only the class `GET_STATUS` fails; `control` already
    tolerates a ShortPacket data stage, so the `0x0d` is the data stage and the
    **status-stage** event then fails the wait — and the ~250 ms logging
    cadence shows the wait rejects *fast*, so a real event arrives that it
    rejects, not a timeout. The remaining gap was that `await_event_for`
    discarded *why* it rejected and *what* it saw; it now records a
    `last_reject` reason (`1` unexpected TRB type, `2` TRB-address mismatch, `3`
    undecodable completion code, `4` budget timeout) and the rejected event's
    raw `last_event_type` (reset per transfer, exposed via
    `UsbDevice::last_reject_reason`/`last_event_type`; behaviour unchanged —
    same fail-closed `Err`, §2.9), surfaced as `evtype_hex`/`reject_hex` on the
    `4127` record. Host-proven by `Trb::trb_type_raw` and
    `faulting_hub_port_status_records_an_unexpected_event_type` (a `GET_STATUS`
    answered by a `NoOp`-type event: the wait fails closed,
    `last_reject_reason()=1`, `last_event_type()=8`, `last_completion_code()` a
    truthful `0`).
  - **Root cause of the `GET_STATUS` faults: the hub's interrupt endpoint was
    armed — fixed.** The metal capture came back `reject_hex=2`
    (TRB-address mismatch) with `evtype_hex=0x20` (a real Transfer Event) on
    ports 1–2, and ports 3–4 with no event (`completion=0`/`reject=0` = the
    EP0 ring had wedged from the earlier faults). Root cause:
    `enumerate_hid` configured, primed, and doorbelled the **interrupt-IN
    endpoint for every enumerated device, including the hub**. A hub's
    interrupt endpoint is its status-change pipe; once armed and doorbelled the
    hub delivers asynchronous status-change reports, and those interrupt
    Transfer Events interleave with the subsequent EP0 hub-class `GET_STATUS`
    control transfers — their TRB pointer is not in the control wait's watch
    list, so `await_event_for` rejects them (`reject=2`), and the faulted
    transfer leaves its TRBs in flight, wedging the EP0 ring for the remaining
    ports. Fixed by configuring/priming/doorbelling the interrupt-IN endpoint
    (and issuing `SET_PROTOCOL(boot)`) **only for a HID interface**
    (`InterfaceInfo::is_hid`): a hub keeps only its control endpoint, which is
    all the downstream-port `GET_STATUS` polling needs, so no async event is
    ever generated to corrupt it. A keyboard plugged directly into the root hub
    is unaffected (its HID interface still arms its report ring). Host-proven by
    `drivers/bus/usb` `enumerating_a_hub_does_not_arm_its_interrupt_endpoint`.
    The metal capture then read clean: `4127` reported real
    `connected_hex`/`speed_hex` per port (the keyboard on a downstream port,
    full speed).
  - **Downstream addressing — done (second xHCI slot, Route String + TT).**
    `UsbDevice::enumerate_downstream_hid(down_port, speed)` addresses the
    keyboard hanging off the hub on a **second** slot: the hub stays addressed
    on its slot (`Layout` reserves a second `output_ctx2`/`ep0_ring2`, and
    `control`/`address_device`/`next_report` follow the active slot via
    `ep0_ring_off`/`output_ctx_off`), and the downstream slot's context carries
    the **Route String** (the hub's downstream port, §8.9) plus — for a
    full/low-speed device behind the high-speed hub — the **TT** Hub Slot ID +
    Port Number (§6.2.2). `slot_ctx_dwords` takes a `SlotCtxBase` (speed,
    root_port, route, TT); the post-Address sequence is the shared
    `finish_enumeration` (identical to a root-port device). A latent bug fixed
    in passing: `control`'s data-stage publish wrote the *first* slot's ring
    offset, harmless for a root device but fatal for the second slot. The kernel
    `usb_keyboard::address_downstream_keyboard` finds the connected port, resets
    it (`SET_FEATURE(PORT_RESET)`, kernel-owned recovery delay), confirms it
    enabled, and addresses it, logging the keyboard under `EventId(4128)`.
    Host-proven by `enumerate_downstream_hid_addresses_a_full_speed_keyboard_through_the_hub`
    (full speed → TT validated), `..._omits_the_tt_for_a_high_speed_device`, and
    `..._before_a_hub_is_addressed_fails_closed`. **Metal: `4128` confirmed** —
    a re-flash logged the keyboard (`3434:0e21`, slot 2, hub port 4, full
    speed). Metal-only beyond the host tests (QEMU models no Pi PCIe/USB, §0.4).
  - **Mark the parent hub as a hub — done (downstream keystrokes flow).**
    With `4128` confirmed on metal, pressing keys still produced nothing: the
    keyboard was addressed but never delivered a report. The hub had been
    enumerated like any device, leaving the **Hub** bit clear in its slot
    context, so the VL805 never scheduled the full-speed keyboard's split
    transactions (Address Device still succeeds, hence `4128` passed). Fix:
    `enumerate_downstream_hid` now calls `configure_hub_slot` before addressing
    the device — it reads the hub descriptor (`read_hub_topology`: `bNbrPorts`
    + `wHubCharacteristics` TT Think Time), copies the hub's live output slot
    context (`read_ctx`), sets the **Hub** bit + **Number of Ports** + **TT
    Think Time** (single-TT, MTT clear), and issues an `A0`-only Configure
    Endpoint over the hub's slot (xHCI §6.2.2). The mock now requires the Hub
    bit on that command and delivers no downstream interrupt report until it is
    set, so `addressing_a_downstream_keyboard_marks_the_parent_hub_as_a_hub`
    and the existing full-speed drain test fail before the fix and pass after.
    **Metal:** a re-flash still typed nothing at the prompt — the keyboard is
    addressed and the hub marked, but no report reaches the console; the next
    bullet instruments the poll loop to localise that.
  - **Keyboard poll-loop diagnostics — done (`4129`/`4130`/`4131`).** With the
    keyboard addressed (`4128`) and the hub marked, typing still produced
    nothing. After bring-up the keyboard service polls forever
    (`pump_once` → decode → `ArbiterConsoleSink`), and that loop **discarded
    its result** (`let _ = pump_once(...)`), so a UART capture could not tell
    whether reports arrive, whether `next_report` faults, or whether the loop
    runs at all. `keyboard_service`'s loop now folds each poll result into
    `usb_keyboard::KeyboardPumpDiagnostics`, which emits three **bounded** audit
    events (§2.16 / §19.4): a one-shot `4129` the first time a report drains
    (keystrokes flow), an on-change `4130` carrying the `DriverError` name when
    `pump_once` faults (capped at 16), and a capped `4131` heartbeat (every 1024
    polls, ≤ 32 total) carrying cumulative `polls`/`events`/`errors`. The metal
    reading splits the failure: `4129` present ⇒ path works; recurring
    `4130 err=device_fault` ⇒ `next_report` rejects the controller's
    interrupt-IN events; `4131` polls climbing with `events=0 errors=0` ⇒ the
    loop is alive but the controller never completes the interrupt endpoint.
    Host-proven by `pump_diagnostics_logs_the_first_report_only_once`,
    `..._logs_a_pump_error_on_change_and_caps_it`, and
    `..._emits_a_bounded_heartbeat`. **Metal:** the re-flash read the `4131`
    branch — the heartbeat climbed (`polls 0x400`→`0x8000`) with
    `events=0 errors=0`, no `4129`/`4130`: the loop polled fine, `next_report`
    never faulted, but the controller serviced the interrupt endpoint never;
    the next bullet fixes that. Docs: `docs/src/platform/aarch64.md`.
  - **Interrupt-endpoint Max ESIT Payload — done (the no-report fix).** The
    `4131 events=0` reading localised the silent keyboard to "addressed but the
    periodic endpoint is serviced never". Root cause: the interrupt-IN endpoint
    context (`ep_ctx_dwords`) left **Max ESIT Payload** zero (§6.2.3.8 dword 4
    bits 16:31). The xHCI periodic scheduler reserves no bus bandwidth for a
    periodic endpoint whose Max ESIT Payload is zero (§4.14.2), so the
    controller scheduled the full-speed keyboard's split transactions through
    the hub's TT *never* — Address Device and Configure Endpoint both succeed
    (hence `4128`), but no report ever flows. The fix programs Max ESIT
    Payload = the max packet size for any periodic (non-zero-Interval)
    endpoint; a control endpoint leaves the field reserved-zero. The mock now
    delivers no interrupt report while the payload is zero, so
    `the_downstream_interrupt_endpoint_carries_a_nonzero_max_esit_payload` and
    every existing report-drain test fail before the fix and pass after.
    **Metal:** the re-flash did **not** change the symptom — `4131` still
    climbed with `events=0`, no `4129`/`4130` — so a non-zero Max ESIT Payload
    is necessary but was not the (only) cause; the next bullet found it. Docs:
    `docs/src/drivers/bus.md`, `docs/src/platform/aarch64.md`.
  - **Interrupt endpoint read from the descriptor — done (the no-report fix
    that held).** With the hub marked and a non-zero Max ESIT Payload the metal
    keyboard was *still* silent (`4131 events=0`). The remaining wrong
    assumption was the endpoint itself: the driver hard-coded the interrupt-IN
    endpoint as endpoint 1 (DCI 3), a fixed interval, and an 8-byte packet, and
    never read the keyboard's endpoint descriptor. A keyboard whose
    interrupt-IN endpoint is not endpoint 1 then has its Configure Endpoint,
    doorbell, and `next_report` all aimed at the wrong DCI, so the controller
    schedules the real endpoint never — Address Device + Configure Endpoint
    succeed (hence `4128`), but no report flows. `InterfaceInfo::decode` now
    walks past the matched interface to its first interrupt-IN endpoint and
    captures its DCI (`2·endpoint_number + 1`), `wMaxPacketSize`, and
    `bInterval`; `finish_enumeration` configures/doorbells/drains that DCI
    (stored as `UsbDevice::int_dci`) and `interrupt_interval` derives the
    endpoint-context Interval from `bInterval` + speed (xHCI Table 6-12) rather
    than a fixed exponent; a HID interface with no interrupt-IN endpoint fails
    closed (`BadMagic`). The mock derives the configured DCI from the Configure
    Endpoint add flags and posts interrupt events with it, so
    `downstream_keyboard_is_serviced_on_its_descriptor_reported_endpoint` (a
    keyboard whose interrupt endpoint is endpoint 2 → DCI 5) fails before the
    fix and passes after. **Metal: confirmed.** The re-flash drove the
    on-screen `Username:`/`Password:` prompt from the USB keyboard — `4129`
    drained the first report and `4131` then climbed with the keystroke count
    (`events` rising as keys were pressed, `errors=0`). The Pi 4B USB-HID
    keyboard path is end-to-end working; the remaining metal residual is login
    itself (`users_db_read err=12` — no users database, P11 follow-up), not the
    keyboard. Docs: `docs/src/drivers/bus.md`, `docs/src/platform/aarch64.md`.
  - **Path forward — the no-touch probe (implemented; `AGENTS.md` §15.7).**
    The user declined the chain-load route, so the gentlest possible
    bring-up is the chosen decisive experiment: `reset_controller` only
    **releases** the bridge `sw_init` the previous boot stage left asserted
    and does **not** re-assert a fundamental reset or toggle the SerDes
    `IDDQ`, leaving any resident VL805 firmware untouched; `train_link`
    deasserts the already-asserted `PERST#` (the single firmware-(re)load
    edge), and the `NOTIFY_XHCI_RESET` reload stays a best-effort fallback
    issued only when config `0x50` (the `4118` firmware-version wait) stays
    `0` — its outcome no longer aborts the bring-up (see the
    firmware-version-gate bullet above; the gate is now the BAR capability
    block at `Xhci::open`).
    Host-proven by `pcie_brcm::reset_releases_sw_init_without_re_asserting_a_fundamental_reset`
    and `bring_up_releases_sw_init_before_touching_misc_and_skips_the_serdes_toggle`.
    Decisive metal measurement: `4110`/`4114` `vl805_fw_version_hex` going
    non-zero (with a live `CAPLENGTH` at `4107`/`4109`) proves `start4.elf`
    left the firmware resident and our earlier reset was destroying it (the
    in-tree fix is then complete); still `0`/`dead_dead` proves the
    bare-metal handoff genuinely never loads it, leaving a boot-chain /
    firmware matter (a chain-loader that loads the
    VL805 firmware once after PCIe config, or a `start4.elf`/`config.txt`
    change keeping PCIe up across the handoff) as the only remaining path.
    Metal-only either way (QEMU models no Pi PCIe/USB, §0.4).
- **`4111` outbound-window read-back — measure the memory path (§15.7).**
  Rather than guess, `bring_up_keyboard` now logs one-shot `EventId(4111)`
  before consuming the trained window: `BrcmPcieRc::outbound_window_readback`
  reads `MEM_WIN0_LO/HI`, `BASE_LIMIT`, `BASE_HI`, `LIMIT_HI` and
  `MISC_PCIE_STATUS` back (fail-closed per-field sentinel, §2.9). The metal
  capture **pinned the root cause**: `mem_win0_base_limit=0x00003ff0`, which
  under the BCM2711 field order decodes to CPU base `0x6_3ff00000` *above*
  limit `0x6_00000000` — an inverted, empty window (see the root-cause bullet
  below). Host-proven:
  `outbound_window_readback_reports_the_programmed_window` (pcie_brcm) +
  `outbound_window_readback_logs_one_4111_record` (usb_keyboard).
- **Resolved (superseded) — reset the controller before touching MISC,
  unconditionally.** `0xdead_dead` is the **BCM2711 root complex's master-abort
  poison** (distinct from RustOS's all-ones `0xffff_ffff` sentinel) — returned
  when a CPU access reaches the RC but no target decodes the address. An
  earlier design read `MISC_PCIE_STATUS` at entry and *skipped* the reset when
  the link was already up, to preserve the bootloader-loaded VL805 firmware.
  The metal captures killed that twice over: the entry status read is itself a
  MISC-block access that master-aborts ~10.8 s **before** the reset (so it
  cannot report the link state — it *is* the bring-up pause, see P4), and
  `entry_link_up` read `0` on every capture because the bootloader hands the
  link off **down** (the standard cold-reset flow), so the skip path never engaged.
  The skip-reset path, `entry_link_up`, and the `4112` log are therefore
  removed (§2.14), and `BrcmPcieRc::bring_up` unconditionally resets, matching
  the BCM2711 PCIe bring-up sequence: cycle the always-accessible RGR1 bridge
  `sw_init` reset (+ `PERST#`) **before** any MISC access, then clear the
  SerDes IDDQ, program the windows, deassert `PERST#`, poll for link-up, ending
  fail-closed (§2.9/§5.4). The firmware the reset drops is reloaded over the
  freshly-trained link via `NOTIFY_XHCI_RESET` (the working flow on this
  board). Host-proven:
  `bring_up_releases_sw_init_before_touching_misc_and_skips_the_serdes_toggle` +
  `bring_up_trains_the_link_and_programs_the_windows`. The pause vanishing on
  metal is an on-metal acceptance item (QEMU models no Pi PCIe/USB, §0.4).
- **Firmware reload sequenced after the VL805's BAR is based (done).** The
  reload fires from `open_controller`, after `map_controller` bases the BAR +
  sets memory/bus-master decode and before the caps wait/`Xhci::open`
  (non-fatal, §2.9/§18.4) — `dev_addr=0x10_0000` and the property message
  match the vendor's VL805 reset message exactly. Its outcome/response
  (`4108`/`4113`) and a post-reload config + cap re-read (`EventId(4114)`,
  `log_post_reload_state`) are logged one-shot. Host-proven:
  `open_controller_reloads_the_firmware_after_the_bar_is_based` and
  `open_controller_skips_the_firmware_reload_when_mapping_fails`.
- **Resolved — the persistent `dead_dead` was an inverted outbound window
  (the firmware-load was a red herring).** With the reload sequenced
  correctly the metal `4114` still showed the VL805 fully present and
  correctly programmed after the reload (`1106:3483`, BAR0 based at
  `0xc000_0000`, mem-space + bus-master enabled) yet the caps stayed
  `dead_dead`; that, plus USB working under other operating systems on the same
  card/firmware, redirected the search from the firmware to the **outbound
  (CPU→PCIe) translation window** (the path config reads do not exercise). The
  bug was in `rustos_drv_bus_pcie_brcm::regs`: the BCM2711 *proprietary*
  `MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT` (`0x4070`) packs the **limit** in bits
  `[31:20]` and the **base** in `[15:4]`,
  but `MEM_WIN0_BASE_LIMIT_BASE_MASK`/`..._LIMIT_MASK` were
  defined with the halves transposed, so `program_outbound_window` wrote the
  base into the limit's half and vice-versa. For the Pi 4 window that yielded
  the metal `4111` value `0x00003ff0` = base `0x6_3ff00000` *above* limit
  `0x6_00000000`: an inverted, empty window decoding nothing, so every BAR
  read master-aborted to `dead_dead` regardless of firmware. The fix swaps the
  two mask constants; the expected Pi read-back is
  `mem_win0_base_limit=0x3ff00000` with `base_hi=limit_hi=0x6`.
  Host-proven by
  `outbound_window_decodes_a_non_empty_range_covering_the_cpu_window`
  (decodes the full CPU base/limit with the *hardware* field positions,
  independent of the named constants; fails before the swap with
  `base 0x6_3ff00000 > limit 0x6_00000000`, passes after). The live VL805
  answering a plausible `CAPLENGTH` remains the on-metal acceptance item (§0.4
  — QEMU models no Pi PCIe/USB).
- **Firmware-load instrument — the VL805 `0x50` version register.** The
  `4110`/`4114` config read-backs now also dump `vl805_fw_version_hex` (the
  VL805 XHCI MCU firmware version at PCI config offset `0x50`), the register
  the vendor firmware-init sequence reads to confirm a load (`0` until
  loaded, a non-zero build id once `VideoCore` loads the blob). It is read
  over configuration space, which works on metal even while the BAR aborts,
  so a metal capture now distinguishes "firmware never loaded" (`0x50`=0,
  a board/firmware matter outside RustOS code) from "loaded but the BAR
  window still does not decode" (`0x50`≠0) directly, instead of inferring it
  from the `dead_dead` BAR. Host-proven by
  `config_readback_dumps_each_register_once`.
- The seam is the new `kernel/core` `InitSpawnCtx::spawn_kernel_service`
  (admits a `spawn_kthread` whose body drives an object-safe `YieldHandle`,
  so it need not name the port's context-switch type) + `static_frames`
  (the leaked `'static` allocator the DMA region is held from for the
  driver's lifetime, §4).

Host-proven: the `platform::pcie_bringup` decoder tests, the
`keyboard_service` mapper/DMA host capability+bounds tests, and the
kernel-core `spawn_kernel_service` admission test; the freestanding
aarch64 kernel builds with the full wiring. The VL805 BAR now answers a
plausible `CAPLENGTH` on metal; the current fix moves `Xhci::open` past the
halted pre-reset `USBSTS=0x805` (`HCH|HSE|CNR`) and reset-stuck
`USBSTS=0x815` (`HCH|HSE|PCD|CNR`) states by clearing only stale
write-1-to-clear status latches before `HCRST`, then enforcing `CNR` after
reset. Host-proven by `open_resets_a_halted_controller_with_pre_reset_cnr_and_hse`.
**Remaining — metal only:** xHCI reset/start completing on the real VL805 and
a USB keyboard driving the video-console login (the §0.4 on-metal checklist).
The architecturally-correct long-term home is still a `devmgr`-autoloaded
userland keyboard *service* (rides the DriverSpawner-over-IPC gap, Stage
4.HW increment 1); the in-kernel service is the interim. Then the DWC2 OTG
path if needed; then the WM/taskbar/session on the HVS path.

**Migrating the chain onto `hwtree` + `devmgr` autoload, then deleting the
composition module — in progress (`PLAN.md` Stage 4.HW item 5).**
`kernel/rustos-kernel::usb_keyboard` is a *scaffold*: the one crate §17.4
lets name the four driver crates of the Pi 4 chain (`pcie_brcm` →
`pci::mechanism_brcm` → `bus_usb` → `input_usb_hid`). It must not become
the model for board support — one hand-written composition module per board
is exactly the §2.2/§2.3 sprawl to avoid. The scaling-correct steady state
is the §18 data-driven path: every chain node is discovered into the
`hwtree` and `devmgr` autoloads the matching driver against its signed bind
table, so a new board is match **data**, not new code. Sub-increments
(`PLAN.md` Stage 4.HW item 5, one fully-gated landing each):
- **5a — done.** Each chain driver crate owns its canonical bind table as
  `pub const BIND_KEYS` (`pcie_brcm` compatible `brcm,bcm2711-pcie`;
  `bus_usb` xHCI PCI class `0x0C0330` vendor/device-wildcard; `usb_hid`
  HID boot keyboard `0x030101` + mouse `0x030102` vendor/product-wildcard),
  `HwMatchKey`'s constructors are `const`, and `HwMatchKey::matches` adds
  the PCI/USB class-with-wildcard semantics `rustos_devmgr` resolves
  against (no `#[repr(C)]` change, no C-header drift).
- **5b** runtime `hwtree` child attachment by the bus drivers. **5b-i —
  done:** `PciBus::describe_function(bdf, parent_id, node_id)` (lib/abi,
  implemented on `Pci<C>`) returns an enumerated function as a child
  `HwNode` carrying one `HwMatchKey::pci` of its `vendor:device` and **full
  24-bit class** read from config dword 2 (`base<<16|sub<<8|prog_if` — the
  16-bit `BusDevice::class` drops prog_if, so the xHCI `0x0C0330` is told
  apart from older USB host classes), `HwDeviceClass` from the base class,
  fail-closed `NotFound` on an absent function (§2.9/§18.5); a new trait
  method only (no `#[repr(C)]`/C-header drift), host-proven that the VL805
  node matches `bus_usb::BIND_KEYS`. **5b-ii — done:** `bus_usb` emits the
  HID device under the VL805 keyed by its **interface** class
  (`0x030101`/`0x030102`). `UsbDevice::enumerate_hid` now reads the
  configuration descriptor and parses its first interface descriptor
  (`InterfaceInfo::decode`, fail-closed): the discovered `bConfigurationValue`
  / `bInterfaceNumber` drive `SET_CONFIGURATION` / `SET_PROTOCOL(boot)` (no
  longer hard-coded `1` / `0`), the 24-bit interface class is captured (never
  fabricated, §18.5), and `UsbDevice::describe_device(parent, node)` returns an
  `Input` `HwNode` with one `HwMatchKey::usb` of `vid:pid` + that class (a new
  method only — no C-header drift), host-proven that the `usb_hid::BIND_KEYS`
  keyboard key resolves against it. The remaining sub-increments turn the
  bring-up *around* — from a module that hunts for the keyboard to
  data-driven discovery + `devmgr` autoload (one fully-gated landing each;
  the live VL805 path is a §0.4 metal-acceptance item, so each touching
  chunk lands host tests + a metal checklist and the operator supplies the
  UART log between chunks):
  - **5c-i — done:** the match policy moved out of `devmgr` into the shared
    **`lib/devmatch`** crate (`resolve`/`best_bind_priority`/
    `DriverCandidate`/`MatchResolution`), the single §18.3 definition the
    kernel reaches without a kernel→userland edge (§2.2 / §17.4; `devmgr`
    re-exports it). The in-kernel production driver-candidate catalogue
    (`kernel/rustos-kernel::driver_catalog`) pairs each chain driver's
    canonical `BIND_KEYS` with its `/System/Drivers/` path (authored from
    the crates' tables, never re-typed), and `keyboard_service` gates the
    bring-up on it: `resolve_discovered_bridge` resolves the discovered
    `brcm,bcm2711-pcie` identity (`platform::PCIE_COMPATIBLE` — the
    discovery contract, never a fabricated key, §18.5) against the catalogue
    and proceeds **only on a bound `Winner`** (audit `EventId(4112)`),
    leaving an unmatched/tied node unbound + logged and the service
    unstarted (§18.4 / §2.9). The kernel no longer hunts: it brings the bus
    up because a driver's bind table matched a discovered node. Host-tested;
    the freestanding aarch64 kernel builds with the gate. **Metal checkpoint
    (operator, §0.9):** re-flash, confirm the on-screen `Username:` prompt
    still takes keystrokes (parity), supply the UART log with the `4112`
    bound record.
  - **5c-ii — done:** the in-kernel chain bring-up is admitted through the
    signed-manifest `drvhost::Host::load` gate, not a bare `register()`
    call. `build.rs` (`emit_signed_driver_manifests`) bakes a signed
    `DriverManifest` for each chain driver — `kind = InKernel`, stamped with
    the kernel's `SYSCALL_TABLE_HASH`, requesting `CAP_DRV_LOAD`, carrying
    the driver crate's own `BIND_KEYS` — Ed25519-signed with the build's
    deterministic driver-signing key (`KERNEL_DRIVER_SIGNING_SEED`), and
    embeds the matching public key as the kernel's sole driver trust anchor.
    `kernel/rustos-kernel::driver_loader::ChainDriverLoader::admit` runs the
    full `Host::load` pipeline (trust-anchor + signature verification,
    syscall-hash match, `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` gates, bind-table
    validation, in-process `register()` hand-off). The chain `register()`s
    are admission-only (§8 capability check), so the gate uses a plain
    `Host` with no MMIO/DMA host; the real register-window mapping + DMA
    carve still run over the keyboard service's own capability-gated
    `ChainHost` after admission. `keyboard_service::spawn_if_present` admits
    `pcie_brcm` + `bus_usb` before bring-up (fail closed → no service), and
    the service body **re-matches the enumerated HID child** against the
    catalogue (`bring_up_keyboard` now returns the keyboard + the
    `UsbDevice::describe_device` `HwNode`) and admits `usb_hid` before the
    report pump (fail closed → no input). Audited at `EventId(4132)`.
    `rustos-drvhost` is now an aarch64 dependency. Host-tested
    (`driver_loader` 5 tests: all three baked images verify, the
    `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` / unknown-path refusals fail closed);
    the freestanding aarch64 kernel builds clean. **Metal checkpoint
    (operator, §0.9):** re-flash, confirm the `Username:` prompt still takes
    keystrokes (parity), and supply the UART log showing the `4132` admitted
    records for `pcie_brcm`, `bus_usb`, and `usb_hid`.
  - **5d-0** the `DriverHost` DMA/MMIO surface reachable **over IPC** (the
    standing gap, Stage 4.HW increment 1's remainder). The arch-neutral
    **security foundation is landed**: a new `abi-v1` syscall **`mmio_map`**
    (no. 26, gated on `CAP_MMIO_MAP`, audited) maps a *granted* device MMIO
    window into the calling driver's own address space. A driver passes an
    unforgeable, kernel-issued device-resource grant **handle** (never a raw
    phys address); the kernel-core handler resolves it **against the calling
    task** through the per-task device-resource grant table (owner-checked
    forgery defence, §5.4), validates the grant names a memory window
    (`devres::mappable_window` — `Mmio`/`BusWindow`), and maps only that
    region through the architecture `devres::MmioMapFacility` producer
    (§18.3 — a driver reaches only the resources its matched node requested;
    §4 — no ambient authority). The map facility fails closed to its NULL
    default (`NULL_MMIO_MAP_FACILITY` → `NotImplemented`), exactly as
    `mem_map` shipped its core handler before its arch producer. **5d-0-ii
    (a) — the concrete grant table — LANDED.** The per-task device-resource
    grants live in `kernel/core::aspace::AddressSpaceRegistry` alongside the
    task's streams and limits (the same per-process lifecycle, so a parallel
    per-task registry + lock is avoided, §2.2): `mint_grant(task, resource)`
    issues a per-task, monotonic, never-reused handle from `1` (handle `0`
    reserved-invalid); `grant(task, handle)` resolves it owner-checked (a
    foreign task or unknown handle → `None`, the forgery defence the handler
    relies on); and `withdraw` reclaims every grant when the task exits. The
    `mmio_map` handler now resolves through `aspaces`, and the placeholder
    `devres::ResourceGrants` trait / `EmptyResourceGrants` /
    `NULL_RESOURCE_GRANTS` / `with_resource_grants` seam from the security
    foundation was deleted in place (§2.13 / §2.14). Host-tested (the grant
    store's 6 unit tests + the 5 `mmio_map` handler tests minting real
    grants); no `lib/abi`/C-header change. **5d-0-ii (b) — the guarded
    borrowed-space MMIO mapper mechanism — LANDED.**
    `kernel/mem::mmio::MmioWindowMap` is the per-task guarded MMIO
    virtual-window allocator (bounded window + slot bitmap + per-region
    guard/data accounting) that maps a device window into a **borrowed**
    `&mut AddressSpace<P>` — caching disabled (`NO_CACHE`), never executable
    (W^X, §19.2), unmapped guard pages bracketing every window (§4), and an
    all-or-nothing fail-closed unwind on a part-way page-table failure
    (§2.9). It is the device-window analogue of `kernel/mem::anon`'s
    `map_anonymous` and the mechanism the production `MmioMapFacility` will
    drive against a task's retained live address space. The existing owned
    `MmioMap` (the in-kernel driver-host register-window mapper consumed by
    `KernelMmioMapper`) is now a thin wrapper delegating to `MmioWindowMap`,
    so the guarded logic has one definition (§2.2) with no consumer churn.
    Host-tested (8 borrowed-space tests + the 15 existing `MmioMap` tests
    still green); no `lib/abi`/C-header change. **5d-0-ii (b′)-1 — the
    arch-neutral live-address-space retention mechanism + production
    producers — LANDED (host-proven).** A task's *live, mutable*
    `AddressSpace<P>` is now retainable and reachable from its own syscall
    path, closing the immutable-`FrozenAddressSpace` gap the producers needed:
    - `kernel/mem::live` (`LiveUserSpace` object-safe `Send` trait +
      generic `LiveSpace<P, M>`) erases the live space behind one boundary
      (so `kernel/core` names no concrete `P`, §17.4), composing the audited
      `map_anonymous`/`unmap_anonymous` + `MmioWindowMap` with no second
      mapping path (§2.2); `LiveSpaceError` unions their errors. 7 host tests.
    - `kernel/core::kthread` retains the boxed space in the task's
      `ThreadControl` and **publishes a per-CPU pointer to it** in a new
      `USER_LIVE_SPACE` table — published before switch-in, cleared on
      switch-back, exactly as the existing `USER_RESUME` handle — so the
      access is exclusive to the one CPU running the (trapped) task; the
      `with_current_live_space(cpu, f)` accessor and the
      `spawn_user_kthread_with_stack_live` admission entry expose it. No
      live page table is ever stored behind a shared lock (the documented
      `!Send`/`!Sync` reason a frozen snapshot was used remains intact).
    - `kernel/core::live_producer` (`LiveMemMap<A>` / `LiveMmioMap<A>`,
      holding `&'static A` like `KernelProcessWait`) are the production
      `MemMap` / `MmioMapFacility` producers: they read `arch.current_cpu()`,
      route through `with_current_live_space`, fold `LiveSpaceError`→`Errno`,
      and fail closed (`NotImplemented`) when the running task has no retained
      space. `mmio_map` is fully served (the device window's placement is the
      guarded `MmioWindowMap`); anonymous `mem_map` is served for `FIXED`
      placement, with the non-`FIXED` per-task user-VA placement allocator
      the remaining `SP5b` follow-on (fail-closed `NotImplemented` until
      then — never a guessed base). 8 host tests. fmt + clippy `-D warnings`
      clean; no `lib/abi`/C-header change.
    **5d-0-ii (b′)-2 — retention wired into production (aarch64) — LANDED.**
    The optional live space is threaded through the `admit_init` /
    `admit_process` seam as `Option<Box<dyn LiveUserSpace + Send>>` (all three
    ports + the in-core test double — x86_64 / riscv64 pass `None`). The
    aarch64 `init_spawn` and `spawn_producer` freeze a snapshot for the copy
    path **and** build a `LiveSpace` from the *same* arch space (device-window
    region 1 GiB above the image bias; anonymous frames from the `'static`
    kernel allocator), admitting through `spawn_user_kthread_with_stack_live`
    so the runtime publishes it on the per-CPU slot. `kernel_main` installs
    `LiveMemMap` / `LiveMmioMap` for **every** port (arch-generic, no `cfg`):
    a task with no retained space (the sibling ports today) fails `mem_map` /
    `mmio_map` closed exactly as the `NULL_*` defaults did. The Arch-HAL
    `PageTableFrames` gained a `Sync` supertrait so a port's `AddressSpace`
    (which retains the frame source) is `Send` and can be the boxed
    `LiveUserSpace` (every implementor was already `Sync`). **Arch fix:**
    `kernel/arch/aarch64::leaf_attrs_for` mapped any `DEVICE` page EL1-only,
    so a user-space driver's `mmio_map` window permission-faulted at EL0; a new
    `el0_device_leaf_attrs` (`AP_RW_EL0`, Device, PXN|UXN) is selected for
    `DEVICE | USER` (regression-tested). Proven on `-M virt` by the
    `mmio_map_qemu_aarch64` vertical: the kernel retains a `LiveSpace`, admits
    the EL0 fixture via `spawn_user_kthread_with_stack_live`, mints a grant for
    the first virtio-MMIO transport, and the program maps it through `mmio_map`
    + reads the `MagicValue` register (`0x74726976`) back. New `rustos_rt::mmio_map`
    wrapper; no `lib/abi`/C-header change. The registry-backed grant
    owner-check (§5.4) is host-proven in `kernel/core`.
    **5d-0-ii (c) — non-`FIXED` `mem_map` placement allocator — LANDED.**
    `kernel/mem::AnonWindowMap` (a per-task user-VA placement allocator: bump
    cursor + free-list of released holes, so a large heap window costs no RAM
    until the frame allocator backs a mapping — §24.1; the placement window is
    address space, the physical backing fails closed as deterministic OOM)
    chooses the base for a non-`FIXED` anonymous mapping. `LiveSpace` carries
    one (`map_anonymous_placed`), composing it with the already-audited
    `map_anonymous`/`unmap_anonymous` (no second mapping path, §2.2);
    `unmap_anonymous` validates + releases the placement record before any
    teardown (fail closed on a wrong base/extent, §5.4). `LiveMemMap::map`
    routes non-`FIXED` requests there while `FIXED` still names `addr_hint`;
    the aarch64 `init_spawn`/`spawn_producer` thread the shared
    `spawn_layout::ANON_WINDOW_OFFSET`/`PAGES` (2 GiB above the image bias,
    above the device window). Host-tested (`AnonWindowMap` 7 + `LiveSpace`
    placement 4 + `LiveMemMap` routing 2) and proven on `-M virt` by the
    extended `mmio_map_qemu_aarch64` vertical (the EL0 program maps its granted
    window **and** round-trips a placed `mem_map`: map → write sentinel →
    read-back → `mem_unmap`). No `lib/abi`/C-header change.
    **5d-0-ii (c) DMA half — LANDED.** New `abi-v1` syscall **`dma_alloc`**
    (no. 27, `CAP_MEM_DMA`, audited) carves a driver's DMA buffer bounded by
    the grant's `addr_limit` over the same retained-live-space +
    owner-checked-grant machinery (`with_current_live_space`, `Dma`-kind
    grant): it resolves the grant owner-checked, validates it via
    `kernel/core::devres::dma_constraint` (rejecting zero/over-max length and
    — until the metal VL805 item — a translating inbound viewport), carves a
    physically-contiguous, zeroed, coherent `RW` buffer below the grant's
    `addr_limit` through the `devres::DmaAllocFacility` producer, returns the
    CPU-VA, and copies the device-visible base (CPU-physical for the
    coherent/`virt` case) out to a user pointer. The guarded carve has one
    definition — `kernel/mem`'s borrowed `DmaWindowMap`, with the in-kernel
    `DmaPool` re-expressed as its owning wrapper (§2.2); `LiveSpace` gained
    `alloc_dma` + a DMA window and reclaims (zeroes + frees) every live DMA
    block on `Drop` at task exit (§4). `LiveDmaAlloc` is installed for every
    port in `kernel_main`; the aarch64 `init_spawn`/`spawn_producer` thread
    the shared `spawn_layout::DMA_WINDOW_OFFSET`/`PAGES` (3 GiB above the
    image bias). Host-tested (`kernel/mem` carve / addr-limit / Drop-reclaim,
    `devres` constraint, the `dma_alloc` handler, the `LiveDmaAlloc`
    producer, `abi-sys` marshalling) and proven on `-M virt` by the extended
    `mmio_map_qemu_aarch64` vertical (the EL0 program now also carves a
    `dma_alloc` buffer and round-trips a sentinel through it). New
    `rustos_rt::dma_alloc` + `ros_sys_dma_alloc`; C header regenerated.
  - **5d** the continuous keyboard *service* in **user space**, autoloaded
    by `devmgr` over the 5d-0 surface, feeding the input-focus arbiter.
    - **5d-1 — the rt-backed `DriverHost` (`lib/drvrt`) — DONE
      (host-proven).** The user-space analogue of the in-kernel keyboard
      service's `IdentityMmioMapper` + frame-allocator DMA host: a driver
      process can no longer reach the kernel frame allocator / identity map,
      so `rustos_drvrt::RtDriverHost` implements `DriverHost` + `MmioMapper` +
      `VirtioHost` over a fixed table of kernel-issued device-resource grants
      (`GrantedResource` = handle + `HwResource`). `map_window` resolves a
      requested `(phys,len)` to the covering grant, maps that grant's whole
      window **once** with the `mmio_map` syscall (cached, §2.16), and
      translates an outbound `BusWindow` BAR's PCIe-bus address to the mapped
      CPU window (§18.1); `alloc_dma_zeroed` carves the device-shared region
      with `dma_alloc` against the DMA grant and mints a `DmaSlab` (device
      base from the grant, optional caller-supplied non-coherent
      `SlabCoherencyFn` — the shim is never synthesised in this
      platform-neutral crate, §2.20). The two syscalls sit behind the
      host-testable `GrantSyscalls` seam (production `RtGrantSyscalls` forwards
      to `rustos_rt`, §2.2); the host adds no authority — every capability +
      bound is re-checked kernel-side and a forged/foreign handle fails closed
      (§4/§5.4/§2.9). Allocation-free (`MAX_GRANTS` array) so it works before
      the SP5b heap. 18 host tests (window resolve, sub-offset, BAR
      translation, map-once, every fail-closed path, DMA carve + coherency,
      multi-grant resolution); registered in §3 + `SUMMARY.md`; docs
      `docs/src/lib/drvrt.md`. **No metal/virt step** (no production
      grant-minter/driver-process consumer yet — that is 5d-2).
    - **5d-2-i — the `resource_grants` grant-delivery syscall — DONE
      (host-proven).** The piece `RtDriverHost` consumes to learn *which*
      handles it holds: new `abi-v1` syscall **`resource_grants`** (no. 28,
      **no capability** — a task reads only its own grants, the §16.6/§24.3
      own-process baseline; unaudited) serialises the **calling task's** minted
      grant set from the same per-task `AddressSpaceRegistry` grant table as
      consecutive `rustos_abi::hwtree::GrantedResource` records (handle +
      `HwResource`, `WIRE_LEN` = 40; the one wire/owning definition, re-exported
      by `lib/drvrt`, §2.2), copies them out through the validated boundary, and
      returns the byte count — `0` for an unbound task (§18.4), `BufferTooSmall`
      rather than a partial list (§2.9), `BadAddress` for an unregistered caller
      (§19.1). `AddressSpaceRegistry::grants_to_le_bytes` does the
      ascending-handle serialisation; `RtDriverHost::from_grants_query` is the
      production constructor that issues the syscall into a fixed `MAX_GRANTS`
      buffer and builds the grant table (`RtDriverHost::new` keeps the
      caller-supplied-slice path for tests/verticals). New
      `rustos_rt::resource_grants` + `ros_sys_resource_grants`; C header
      regenerated. Host-tested (abi round-trip + decode-reject, 5 kernel-core
      handler tests, 4 drvrt `from_grants_query` tests, abi-sys marshal). **No
      metal/virt step** (no production grant-minter / driver-process consumer
      yet — that is 5d-2-ii).
    - **5d-2-ii (a) — the production driver-spawn grant minter — DONE
      (host-proven + `-M virt`).** The privileged driver-spawn path now mints
      the spawned driver's device-resource grants at admission: `KernelSpawnCtx`
      carries a `grants: &[HwResource]` (the matched node's requested
      resources, kernel-sourced — never an untrusted caller, §4), and
      `admit_process` calls `AddressSpaceRegistry::mint_grant(child, resource)`
      once per resource after the child is fully registered, keyed to the
      child's own kernel-trusted id (owner-checked, monotonic handles from 1,
      reclaimed on exit). The ordinary `spawn` syscall passes an **empty**
      slice — a user task grants no device windows (§4/§5.2). The child reads
      its handles back through `resource_grants` (5d-2-i). Host-tested in
      kernel/core (mint-per-resource, owner-check, `GrantedResource`
      serialisation, the empty-grant user-spawn case) and proven on `-M virt`
      by the extended `driver_spawn_qemu_aarch64` vertical: the stub is spawned
      through the production `KernelSpawnCtx`/`spawn_with` with a granted
      MMIO window, enumerates it via `resource_grants` (handle 1, MMIO,
      non-zero length), and refuses to reply on any shortfall — so the host
      PASS proves the spawn minted and delivered the grant. No `lib/abi`/
      C-header change (the grants are a kernel-side ctx field).
    - **5d-2-ii (b-1) — the `devmgr`-driven driver-spawn path — done
      (host-proven + `-M virt`).** `devmgr::DriverLoader::load` gained a
      `resources: &[HwResource]` argument that `DeviceManager::autoload`
      sources from the matched `HwNode::resources`, realising §18.3 (a loaded
      driver receives only the resources its matched node requested). The
      production loader `kernel/rustos-kernel::driver_spawn_loader::
      SpawnDriverLoader` (impl `devmgr::DriverLoader`) runs the signed
      `drvhost::Host::load` gate on the discovered `kind = UserSpace` image and
      spawns the verified payload through the arch `DriverProcessSpawn` seam,
      threading those resources into `KernelSpawnCtx.grants` (the (a) minter).
      Host-tested with a recording spawn double; proven end to end on `-M virt`
      by the extended `driver_spawn_qemu_aarch64` vertical (discovered virtio
      node → `13001` node-bound → signed gate → spawn → grant read back via
      `resource_grants` → `4302` PASS). **Security hardening (§2.17):** the
      `drvhost` manifest signature now covers the **payload**, so a spawned
      driver's program is authenticated (regression `tampered_payload_refused`);
      empty-payload in-kernel images are unaffected.
    - **5d-2-ii (b-2-i) — `lib/usb` extraction — done (host-proven + whole
      gate).** The §17.4 layering forbids a `drivers/*`/`userland/*` crate from
      depending on another `drivers/*` crate, so an arch-neutral user-space
      keyboard driver could not compose `drivers/bus/usb` (xHCI) with
      `drivers/input/usb_hid` (HID decode) while the xHCI protocol sat inside
      the bus driver. The bus-agnostic xHCI protocol (`XhciHost` register seam,
      `Xhci` controller engine, TRB/ring vocabulary, single-device HID
      `UsbDevice` enumeration) therefore moved into a new `lib/usb`
      (`rustos-usb`, `lib/abi`-only, `no_std`, Tier-1-portable) — the USB
      analogue of `lib/virtio` ↔ `drivers/bus/virtio` (§2.2/§6/§17.4).
      `drivers/bus/usb` keeps only the §8 `register`, the §18.3 `BIND_KEYS`, and
      the PCI BAR/DMA `wiring` over `rustos_usb`; the kernel scaffold + `wiring`
      repoint to `rustos_usb::{Xhci, device::*, regs}`. The 81 USB tests split
      with the code (71 protocol `lib/usb` + 10 driver `register`/bind/wiring);
      whole gate green (`cargo xtask ci` incl. both Pi images, `fuzz --secs 5`,
      `soak both`, `cargo xtask test --qemu`). Docs: `docs/src/lib/usb.md`,
      `docs/src/drivers/bus.md`, the two crate READMEs, AGENTS.md §3 + SUMMARY.
    - **5d-2-ii (b-2-ii) — generic boot-keyboard orchestration + shared
      `Delay` seam — done (host-proven).** The arch-neutral
      root→hub→downstream-HID bring-up sequence is now one definition,
      `rustos_usb::device::UsbDevice::enumerate_boot_keyboard(delay)` in
      `lib/usb` (§2.2/§18): enumerate the first connected root-hub port and,
      when it is a hub (the Pi 4B onboard hub), power its ports, settle, find
      the connected one, reset it, settle, and address the device behind it on
      a second slot — discovered, never a guessed port, failing closed. Its
      timed settles use the microsecond `Delay` seam, hoisted from
      `drivers/bus/pcie_brcm` into `lib/abi` (`rustos_abi::Delay`) so the PCIe
      and USB driver crates share one trait (§2.2; `pcie_brcm` re-exports it,
      callers unchanged; a trait, so no C-header change). The in-kernel
      `keyboard_service` scaffold's `bring_up_keyboard` now calls the shared
      routine and its duplicated `log_hub_ports`/`address_downstream_keyboard`/
      `log_downstream_keyboard` + the `4127`/`4128` event-ids are deleted
      (§2.2/§2.14). Host-proven (`lib/usb` 74 tests incl.
      `enumerate_boot_keyboard_{returns_a_directly_attached_keyboard,
      descends_through_a_hub_to_the_keyboard,
      fails_closed_when_a_hub_has_no_connected_downstream}`; kernel lib tests
      green). Docs: `docs/src/lib/usb.md`, `docs/src/platform/aarch64.md`.
      Touches the metal-confirmed scaffold bring-up (behaviour-equivalent by
      construction) ⇒ an operator §0.9 metal re-verify (parity: the on-screen
      `Username:` prompt still takes keystrokes).
    - **5d-2-ii (b-2-ii) — arch-neutral boot-keyboard orchestration — done
      (host-proven).** `drivers/input/usb_hid::service::bring_up_boot_keyboard`
      is the composition the user-space keyboard driver runs at start-up. Over
      its `DriverHost` (the rt-backed host built from its kernel-issued grants)
      it carves the device-shared DMA region and aperture-checks it *before* any
      register is touched (fail closed, §5.4), maps its granted xHCI register
      BAR, brings the controller up (`rustos_usb::Xhci::open` +
      `UsbDevice::start`, carving the shared `rustos_usb::XHCI_DMA_BYTES` —
      hoisted from `bus_usb::wiring` into `lib/usb`, §2.2), and runs the
      arch-neutral `enumerate_boot_keyboard`, returning a `BootKeyboard` the
      service loop drives with `pump_once`. It names no PCI/BCM2711/board
      (§2.20): the board PCIe root-complex bring-up + BAR assignment stay in the
      separate board bus driver, and the keyboard node is granted only its
      already-assigned BAR + a DMA constraint (§18.3). `usb_hid` now depends on
      `lib/usb` (a lib, §17.4). Host-proven (6 `service` tests: the cap-missing /
      no-mapper / no-DMA-host refusals, a DMA carve above the aperture and a
      DMA-alloc failure refused, and the all-valid path reaching the controller
      hand-off where the inert mock window faults `DeviceFault` — the metal
      boundary, mirroring `bus_usb`'s `wiring` tests). No `lib/abi`/C-header
      change; whole gate green. Docs: `docs/src/drivers/input.md`,
      `docs/src/lib/usb.md`, both crate READMEs.
    - **5d-2-ii (b-2-iii) (in progress)** the `devmgr`-autoloaded keyboard
      driver `rxe`.
      - **Userland clock + `Delay` prerequisite — done (host-proven).**
        `rustos_rt::clock_get` (the first-party wrapper over `abi-v1` syscall 7,
        the raw `u64` nanosecond reading, no coarsening of its own) and
        `rustos_rt::ClockDelay` — the one userland `rustos_abi::Delay`
        implementation (`delay_us` parks cooperatively via `clock_get` +
        `yield_now`, never a hard spin §2.1; `now_us` floors the reading to
        whole microseconds) — live in the single userland runtime so every
        driver process shares one clock-backed `Delay` rather than each rolling
        its own (§2.2). This is the `Delay` the keyboard-driver binary hands to
        `service::bring_up_boot_keyboard`. Host-proven (`rustos-rt` tests: the
        `clock_get` trap marshalling via the `abi-trap` seam, `now_us`
        flooring, and the cooperative-wait core `spin_until_ns` — past-deadline
        returns without yielding, advancing-clock yields a bounded count). No
        `lib/abi`/C-header change (the syscall already existed).
      - **Remaining:** the binary that builds `RtDriverHost::from_grants_query`,
        derives its BAR + DMA-aperture from its delivered grants, runs
        `service::bring_up_boot_keyboard` then loops `pump_once` (a `ConsoleSink`
        over `key_inject`, the `ClockDelay` above, yielding between polls)
        injecting key edges via `key_inject`; plus the production boot wiring
        that runs `DeviceManager::autoload` against the discovered tree (hosted
        in kernel/core, which owns the scheduler) and the metal checkpoint.
  - **5e** delete `usb_keyboard.rs` + `keyboard_service.rs` (§2.14) and evict
    `usb_hid` from `driver_catalog::IN_KERNEL_DRIVERS` (§18.6) once the
    generic path drives the chain end to end on metal.

**Done when:** on real hardware the desktop composites through `rpi_hvs`,
the taskbar renders, and a USB keyboard/mouse drives the WM; a recorded
demo (photo + UART log) is the acceptance artefact. Headless `-M raspi4b`
CI stays green throughout.

### P11 — Login on the consoles `[~]`

Every *text* console (screen, UART) that reaches user mode sits at a
`login:` prompt; an authenticated user's **shell of choice** is started as
their session. The video console and the UART console are **separate
session contexts** (separate stream backings, separate login instances), so
two users — or the same user twice — can be logged in concurrently.

**Landed — the credential foundation (host-proven):**

- `lib/crypto`: PBKDF2-HMAC-SHA256 (`pbkdf2_sha256` / `pbkdf2_sha256_verify`,
  published vectors, `ct_eq` comparison) — the password derivation the user
  database stores.
- `lib/users` (`rustos-users`): the `/System/Security/Users` `users-v1`
  format — full §5.1 account identity (username, uid/gids, display name,
  home, shell of choice, `CAP_*` grant ceiling, `active`/`locked` state,
  salted PBKDF2 record at a bounded per-record cost), fail-closed bounded
  parser (64 KiB / 512-byte lines / 512 records, unique usernames + uids),
  exact-round-trip serialiser, and `authenticate` with one indistinguishable
  refusal + a dummy derivation at the database's highest cost for unknown /
  locked accounts (§19.1). Fuzz harness `fuzz_users` enrolled in
  `cargo xtask fuzz`. Docs: `docs/src/lib/users.md`.
- `userland/session/login::auth::UsersAuthenticator`: the production
  `Authenticator` seam over a parsed `UsersDb`; every refusal is the same
  `Errno::PermissionDenied`. Login's `Uid`/`Gid` now come from `lib/users`.
- `tools/mkimage` image profiles: `cargo xtask image --target aarch64-rpi
  [--profile debug|installer]` emits
  `images/rustos-aarch64-rpi-<profile>.img`. The **debug** image seeds
  `/System/Security/Users` with the `root`/`root` test account (per-build
  random salt, default cost, explicit admin cap ceiling); the **installer**
  image seeds none (the §11 installer authors it on first boot). Proven by
  mkimage host tests mounting the built root and authenticating.
- **Root-volume read path at boot** (former increment 1):
  `rustos_kernel_core::users::load_users_db` reads
  `/System/Security/Users` off the mounted root volume's
  `FilesystemRead` + `FilesystemSecurity` driver through the VFS's
  §5.3-checked per-inode delegation — the root mount carries the volume's
  driver (`MountTable::back_root`, exactly once), the file is bounded
  against the format's 64 KiB maximum *before* reading, and the bytes go
  through the fail-closed `rustos-users` parser. The read runs under the
  kernel bootstrap identity (`uid 0`, **no** capabilities — a
  capability-gated or unreadable record refuses, §5.1/§5.4); every
  outcome is audited (`USERS_DB_LOADED` 4040 / `USERS_DB_REJECTED` 4041)
  and any refusal leaves **no** database. Proven by kernel/core unit
  tests (every refusal), the `rustfs_image` users-root fixture round
  trip through the real driver, and the `users_db_qemu_aarch64` `-M
  virt` vertical (virtio-blk MMIO → rustfs mount → loader →
  authenticate). The Pi's metal root mount (P8/P9) and the
  volume-key hand-off to the loader on metal ride the P8/P9 metal
  items.
- **The login `Run` binary + the `users_db_read` delivery seam**
  (former increment 1): the login service ships at
  `/System/Services/login` (`userland/session/login/src/run.rs`, a
  `rustos-rt` program) and PID 1 `init`'s `session` directive points at
  it. The kernel-held database is delivered through the new `abi-v1`
  syscall **`users_db_read`** (no. 19, gated on the new
  **`CAP_USERS_READ`** (21), audited): the kernel serves the exact
  `users-v1` text from the `kernel/core::users::UsersDbSource` seam
  (installed via `with_users_db`; fail-closed `NotImplemented` unwired /
  `NotFound` with no database / `BufferTooSmall` rather than truncate),
  and login re-parses it with the same fail-closed `rustos-users`
  parser. With no database (installer image, no root volume) login wires
  a deny-all authenticator — the prompt stays up and every attempt is
  refused (§5.4.5). The `SessionLauncher` spawns the authenticated
  record's **shell of choice** via `spawn`/`wait`; the embedded-program
  registry now carries **per-program capability grants + argument
  vectors** (`EmbeddedProgram.caps`/`.args`, all three arch producers —
  login holds the console pair + `CAP_PROC_SPAWN` + `CAP_USERS_READ`,
  the shell only the console pair). Proven by kernel/core +
  kernel/syscall + login unit tests and the reworked
  `spawn_session_qemu_{aarch64,x86_64}` verticals (init supervises
  login; the aarch64 vertical holds an ordered scripted dialogue over
  the runner's multi-step serial script — `root` → `Password: ` →
  refused password → `Login incorrect` → second `Username: ` → a
  513-byte over-bound line → fail-closed exit → reap → relaunch — and
  the runner fails the run if the guest exits before every scripted
  prompt appeared, so a login crashing per keystroke cannot pass on
  relaunch event counts alone).
  Login's **entire prompt/credential input path is allocation-free** —
  the userland heap's production `mem_map` producer is staged
  (`plans/SPAWN.md` SP5b), so any allocation there would abort the
  process (the original per-keystroke `Vec::push` did exactly that on
  metal: every typed character killed login and `init`'s relaunch
  re-printed `Username: `). `rustos_login::Prompt` fills caller stack
  buffers (`INPUT_LINE_MAX` = 512), `Credentials` borrows `&str`, and
  `Login::run` zeroes the password buffer after every attempt; only the
  authenticate path (which parses a delivered database) still needs
  SP5b and the P8 root mount.
- **Beacon + bring-up debug removal** (former increment 2): the
  boot-progress beacons (`boot_aarch64`/`serial`/`video`) and the
  serial bring-up mirror in `kernel/arch/aarch64/src/serial.rs` are
  deleted. The **boot-log** path routes by build profile
  (`serial::ConsoleWriter`): a **release build** is video-first with the
  UART as the fallback (`AGENTS.md` §10), while a **debug build**
  (`cfg(debug_assertions)`) routes the whole log/debug stream to the
  **UART instead** — even while a login session owns the UART — so a
  serial capture of a development boot carries the full diagnostic
  stream while the screen stays clear for the user-facing session; with
  no UART discovered the bounded transmit drops the bytes and the
  screen is never the debug log's sink. Because the single freestanding
  kernel cannot read which image it was planted in, the routing is tied
  to the **image profile** by building the kernel in the matching Cargo
  profile (`tools/xtask` `kernel_build_profile`): `--profile debug`
  compiles a `dev` (`debug_assertions`-on) kernel that logs to the UART,
  `--profile installer` compiles a `--release` kernel that logs on
  screen. The earlier defect was building both images from one
  `--release` kernel, so `debug_assertions` was always off and the debug
  log never reached the UART.
- **Separate console contexts — LANDED** (former increment 1, minus
  echo). The video console and the UART are independent stream backings
  with their own login sessions:
  - `rustos_abi::DescriptorTable` records, per standard descriptor, the
    installed-console index backing it (`standard_on(console)`;
    `standard()` = console 0). `spawn` (now 3-arg) takes a `console`
    selector — `CONSOLE_INHERIT` (all-ones sentinel) copies the
    caller's own table (login's shell stays on login's console), any
    other value names a validated installed-console index and fails
    closed with `NotFound` otherwise. New `abi-v1` syscall
    **`console_count`** (no. 20, `CAP_CONSOLE_WRITE`, unaudited)
    reports the installed-list length. C view regenerated
    (`ros_sys_spawn` 3-arg, `ros_sys_console_count`,
    `ROS_CONSOLE_INHERIT`).
  - kernel-core holds a `'static [ConsoleDevice]` list
    (`BootInfo::with_consoles` → `KernelSyscallHandlers::with_consoles`;
    empty fail-closed default). `stream_write`/`stream_read` resolve
    the descriptor's direction first, then its console index against
    the list (missing console → `NotImplemented`); the init pipeline
    wraps every listed read half in `BlockingConsoleRead`.
    `KernelSpawnCtx` carries the spawner-resolved table to admit.
  - aarch64 installs `[VideoConsole, UartConsole]` when the P7b
    framebuffer console is active, else `[UartConsole]`.
    `serial::write_console_bytes` is now UART-only (the UART console's
    write half, its own login); `VideoConsole` writes through
    `video::write_bytes` and reads from the **keyboard seam** — a
    directly attached USB-HID / PS/2 keyboard once the P10 input wiring
    lands; until then every poll reports "no input pending" and the
    reader parks at its prompt rather than borrowing the UART's bytes.
    x86_64 (COM1) and riscv64 (SBI) list single write-only consoles
    with fail-closed `NULL_CONSOLE_READ` read halves — behaviour
    unchanged.
  - PID 1 `init` supervises **one login per discovered console**
    (`userland/system/init/src/supervisor.rs`, host-tested over the
    `Sessions` seam): `console_count` → `spawn_at(session, console)`
    fan-out, wait-any reaping, relaunch on the exited session's own
    console within a per-console `SESSION_SPAWN_BUDGET`, exhaustion /
    spawn / wait failures and a zero-console system fail closed
    (`EXIT_NO_CONSOLES` 74). The bootstrap slot table is a fixed
    8-entry stack array until the userland heap (SP5b) lets it size
    from the count.
  - Proven by kernel-core unit tests (per-descriptor console routing
    both directions, console_count, spawn explicit/invalid/inherit
    attachment), lib/rt + abi-sys marshalling tests, and the init
    supervisor host tests; the `-M virt` verticals ride the unchanged
    single-UART list.
- **Stream-layer echo + echo control — LANDED.** Terminal local echo is
  the kernel's read line-discipline behaviour, not a per-program job
  (§2.2): `ConsoleDevice` carries a per-console `echo` flag (default
  on), and `stream_read` writes the bytes it consumes back to the same
  console's write half, rendering a bare CR/LF as CR-LF, so a typed
  username is visible. The **echo-control contract** is the new `abi-v1`
  syscall **`stream_echo`** (no. 21, `CAP_CONSOLE_READ`, unaudited):
  `stream_echo(fd, enabled)` toggles the resolved input console's echo;
  `login` disables it around the password read and restores it after, so
  a credential is never rendered, and fails the read closed if echo
  cannot be disabled (`AGENTS.md` §5.4). First-party wrapper
  `rustos_rt::set_echo`; C stub `ros_sys_stream_echo` (header
  regenerated). Proven by kernel-core tests (echo to the write half +
  CR/LF translation, `stream_echo` disabling echo, fail-closed on a
  non-read fd), console.rs `echo_bytes` unit tests, and lib/rt +
  abi-sys marshalling tests.
- **Read-line editing (Backspace rub-out) — LANDED.** The read line
  discipline now edits the line, not just echoes it. The erase vocabulary
  is one shared `lib/vt` definition (`control::is_line_erase` — Backspace
  `BS` or Delete `DEL` — plus the `ERASE_ECHO` `BS SP BS` rub-out, §2.2), so
  the kernel echo and the reader's buffer can never disagree on which byte
  erases. Kernel **echo** half (`ConsoleDevice::echo_bytes`): an erase rubs
  out the previous character instead of painting a stray control glyph,
  bounded by a per-console column (`echo_col`, reset on CR/LF and on every
  `set_echo` toggle) so a Backspace at the start of the input line never
  walks back over the prompt; the column persists across the many per-byte
  `stream_read` drains one logical input line spans. Reader **buffer** half
  (`rustos_login::push_line_byte`, a host-tested allocation-free helper):
  CR/LF completes the line, an erase pops the last byte (zeroed on removal,
  §4) and is never stored, any other byte appends or fails closed
  `TooLong`; `login::run::read_line_raw` drives it. Proven by `lib/vt`
  control tests, six new `console.rs` erase tests (rub-out, BS-as-erase,
  no-op at line start, column persistence across calls, CR/LF reset,
  `set_echo` reset), and nine `rustos_login::line` tests. Docs:
  `docs/src/architecture/syscalls.md`, `docs/src/lib/vt.md`,
  `docs/src/userland/login.md`.
- **Keyboard input for the video console — kernel-side delivery seam
  LANDED.** The video console's read half is now a kernel-side type-ahead
  queue a keyboard-input driver feeds, not the inert `Ok(0)` poll a
  display-with-no-keyboard returned. New `abi-v1` syscall
  **`console_input`** (no. 22, gated on new **`CAP_INPUT_INJECT`** (22),
  unaudited): a driver that has decoded a directly attached keyboard
  pushes the decoded console bytes into a target installed-console index;
  the kernel copies them in (capability- and bounds-checked, §5.4),
  enqueues them on that console's `rustos_kernel_core::ConsoleInputQueue`
  (a bounded type-ahead ring that is both the console's `ConsoleRead`
  half — drained by a video-login `stream_read`, waking a reader parked
  in `BlockingConsoleRead` — and its `ConsoleInput` half), and zeroes
  each byte as the consumer drains it (a typed password transits it,
  §4 / §23.1). `ConsoleDevice` gained an `input` half (default
  `NULL_CONSOLE_INPUT`, fail-closed; preserved across the init
  `BlockingConsoleRead` rebuild); aarch64's `VIDEO_AND_UART_CONSOLES[0]`
  is backed by the shared `VIDEO_KEYBOARD` queue, while the UART console
  keeps `NULL_CONSOLE_INPUT` (a `console_input` to it fails closed) so the
  video login takes input only from its own keyboard, never the serial
  line. First-party wrapper `rustos_rt::console_input`; C stub
  `ros_sys_console_input` (header regenerated). Proven by kernel-core
  queue unit tests (FIFO drain, short read, ring wrap, overflow short
  push) + the `console_input` handler tests (push→read round trip;
  fail-closed for an unknown console and for a non-injectable UART
  console) + lib/rt / abi-sys marshalling tests.
- **Keyboard input for the video console — host-side producer LANDED.**
  The producer that turns decoded key events into the bytes
  `console_input` injects is now host-proven. The shared terminal key
  map is the new `lib/keymap` crate (`rustos-keymap`): `encode_key(Key,
  Modifiers, &mut [u8])` writes the console (tty) bytes one key press
  sends — a printable char (UTF-8, with the `Ctrl` C0-control and `Alt`
  meta-prefix arithmetic), `Enter`→CR, `Backspace`→DEL, `Tab`, `Escape`,
  the arrows (`ESC [ A`..`D`), and the editing/nav/function keys via the
  canonical `lib/vt` `SS3`/`CSI … ~` tables (no second escape definition,
  §2.2). It is `no_std`, allocation-free (writes a caller buffer, so it
  works before the SP5b userland heap), and fail-closed (`MAX_KEY_BYTES`
  bounds it; an unmappable key emits nothing). `drivers/input/usb_hid`
  gained a `console` module: the US HID-usage→`rustos_input::Key` table
  (letters/digits/shifted symbols/named keys/keypad), a stateful
  `KeyboardConsole` tracking the modifier bits + caps/num lock, and
  `pump_once` — the driver loop that polls the keyboard, feeds each event
  through `KeyboardConsole::feed` + `encode_key`, and injects the bytes
  through a `ConsoleSink` (on metal a `console_input` call against the
  video console's index; host tests use a recording sink). The
  HID-usage→`Key` half is HID-specific (a `ps2` keyboard resolves
  scancode set 1 into the same vocabulary, reusing `lib/keymap`); the
  `Key`→bytes half is the one shared map (§2.2). Proven by 13 `lib/keymap`
  unit tests + 13 usb_hid `console` tests (layout, ctrl/alt, caps/num
  lock, named/arrow/function sequences, fail-closed cases, and the full
  `BootKeyboard`→keymap→sink "hi" chain). Docs:
  `docs/src/lib/keymap.md`, `docs/src/drivers/input.md`.

**Remaining (next increments, in order):**

1. **Keyboard input for the video console — VL805/xHCI metal delivery.**
   The kernel delivery seam (`console_input` + `ConsoleInputQueue`) and
   the host-side producer (`lib/keymap` + the usb_hid `console` module
   above) are landed; what remains is the **metal** path that feeds the
   producer real reports: the USB-HID-over-xHCI **VL805** wiring (the P10
   alternative track) so the driver's `pump_once` loop runs against a
   real keyboard and injects into the video console. QEMU models no Pi
   USB, so this is a metal checklist (`AGENTS.md` §20 / §0.4; the UART
   stays its own session).
2. **Configurable log policy** — the log output/direction (which
   consoles/sinks receive log lines), rotation, and on-storage age
   limits become administrator-settable configuration under
   `/System/Settings` (§16.2), replacing the compiled-in routing;
   requires the persistent `/System/Logs` store (§19.4) before rotation
   and age limits are meaningful. The debug-build dual echo stays a
   debug-only exception.
3. **Login over a real database in production.** The passphrase-derived
   root-unlock **primitive is landed**: `drivers/filesystem/rustfs`'s
   `unlock` module (`UnlockDescriptor` — PBKDF2-HMAC-SHA256 over a
   per-volume random salt + bounded iteration count, fail-closed
   encode/decode, `derive_volume_key`) turns an operator passphrase into
   the volume's `VolumeKey` (`AGENTS.md` §11). It is the LUKS-style
   indirection above the always-encrypted volume; the plaintext
   descriptor rides beside the volume (the FAT boot partition on a Pi
   image). A wrong passphrase derives the wrong key and `RustFs::open`
   refuses it (`PermissionDenied`) — no separate oracle. Host-proven:
   the `unlock` unit tests plus an end-to-end rustfs test that formats a
   volume under a passphrase-derived key and re-mounts it (wrong
   passphrase refused). Docs: `docs/src/filesystem/rustfs-spec.md` §7
   (incl. the §19.9 TPM/secure-boot future hand-off, which seals the key
   to a measured boot and falls back to the passphrase).

   **Image authoring is landed** (`tools/mkimage`): `build_rpi_image`
   provisions a per-volume `UnlockDescriptor` (random salt +
   `UNLOCK_DEFAULT_ITERATIONS`), derives the volume key from the blank
   `IMAGE_PASSPHRASE` (both profiles — these are special-case images:
   the debug image never ships, the installer image is re-provisioned at
   install time), provisions the encrypted root under that derived key,
   and plants the plaintext descriptor on the FAT boot partition as
   `root.unlock` (`fatboot::ROOT_UNLOCK_NAME`). The obsolete raw
   `--root-key` *input* is removed (a supplied key could not match the
   on-image descriptor); the derived key is still emitted to `.rootkey`
   for host mounting and is re-derivable from `root.unlock` + the blank
   passphrase. Host-proven by the mkimage tests (the on-FAT descriptor
   re-derives the exact key; a wrong passphrase is refused with no
   separate oracle, §5.4). Docs: `docs/src/install/raspberry_pi.md`.

   **Still staged** (each its own increment; the chain end to end is
   gated on these):
   - **Installer-authored production root.** The §11 installer first-boot
     flow provisions the *user's* encrypted root under their **chosen**
     passphrase and writes its descriptor — a real, operator-set
     passphrase, never the blank `mkimage` default. (The blank-passphrase
     `mkimage` images above are the development/first-boot artefacts only.)
   - **Boot mount.** The production boot reads the descriptor from the
     boot partition, prompts for the passphrase on the console, derives
     the key, mounts the discovered root volume (EMMC2 on metal /
     virtio-blk on `virt`), runs `load_users_db`, and installs the held
     text via `with_users_db` (kernel-side `load_users_db` exists;
     EMMC2 metal mount rides P8).
   - **Login parse.** The userland `login` parses the served text, which
     needs the production `mem_map` producer (`plans/SPAWN.md` SP5b).
   - A `-M virt` vertical mirroring `users_db_qemu_aarch64` then proves
     `root`/`root` end to end at the prompt over the passphrase path.

**Done when:** a metal Pi 4 and the `virt` verticals sit at `login:` on
every text console, `root`/`root` logs in on a debug image and gets the
record's shell, a second login on the other console works concurrently,
an installer image refuses every login until the installer has authored
users, and no beacon/debug output remains in production boots.

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
                          # model-check, spec-review, abi-check, and the image gate
                          # (both aarch64-rpi profiles built end-to-end)
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
