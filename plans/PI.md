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
  - **P6c-2 — MMU + `kernel_main` hand-off `[x]`.** `boot_aarch64` now
    enables the stage-1 identity MMU (`AddressSpace::new_identity_gigapages`
    over a static boot `PageTablePool`, 512×1 GiB — first GiB Device, rest
    Normal — then `switch`) and installs the EL1 vectors *before* any
    further work, so the `kernel_core` allocator/scheduler atomics run on
    Normal memory and the full-tree `first_memory_region` FDT walk is
    MMU-on-safe (the §watch-out hazard). It adds the local `Aarch64BinArch`
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
    `init_spawn` seam builds a 2 GiB-identity user address space (64 GiB
    bias avoids the gigapage collision), parses the embedded `rxe`, and
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
  hardware-isolated 2 GiB-identity address space from a static
  `PageTablePool` reserve (without switching the spawning caller's
  `TTBR0_EL1`), drives the audited `spawn_image` + `admit_process`, and is
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
    `.with_console`. This completes the **bootstrap backing** — it feeds
    fd 0's backing object (P6e-3a), it is **not** called directly by the
    shell. The receive-bit decoders are host-unit-tested (2 new
    `console` tests + 1 `arch_wrapper_aarch64` adapter test); the
    freestanding aarch64 kernel builds clean. Real RX over silicon is
    exercised once the stream layer binds fd 0 (P6e-3a).
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
      (clean exit); *blocking* is the stream backing's job (§20), so live UART
      RX stays an on-metal item (P6e-2). The `RtProcessHost` launches a single
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
      stays green (the session now writes its gated prompt, reads end-of-input,
      and exits). Docs: `docs/src/userland/shell.md`.
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
      its doc was updated. Real UART **RX** over fd 0 (so the session blocks
      instead of exiting at end-of-input) stays an on-metal item — there is no
      deterministic `-M virt` serial-RX injection (consistent with
      P6e-2/P6e-3a). Docs: `docs/src/userland/init.md` ("Session supervision").
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
    riscv64 + x86_64 report honest `Pending` (their Sv39 / four-level
    huge-page splits land with each port's own guard-page fault-form); the
    `kernel/mem` `HostPageTable` double + `from_map_error` carry the new
    cases (`PageTableError::Unsupported`). Host-proven: the `mmu`
    conformance suite gained a block-split honesty check (declaration
    justified; non-`Supported` ports fail `split_block` closed — 4 new
    arch-api tests), aarch64 `paging_tests` proves the HAL method reaches
    the inherent body over `dyn AddressSpace`, riscv64 proves
    `Pending` + fail-closed. Doc: `docs/src/platform/aarch64.md`
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
    the HAL method reaches the inherent body over `dyn AddressSpace`, and
    riscv64 (Pending) proves the arena fail-closed beside its `split_block`
    one. Host-proven; **no QEMU vertical needed** — G2 already proves the
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
    - **G3b-2-i — PID 1 (`init`) path `[x]`.** A forward-only bump
      allocator `stack_arena::KTHREAD_STACK_ARENA` (`rustos-kernel`) hands
      kthread kernel stacks out of the boot-reserved arena (`boot_aarch64`
      `install`s it from the carved `(base, len)`); each `ArenaStack` is a
      one-page guard below the usable `KTHREAD_STACK_BYTES` stack, identical
      in geometry to `BoxStack`, never reclaimed (§2.1). `init_spawn`
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

### X — x86_64 concurrent user mode: timeshare → spawn → wait (P6 cross-port follow-on) `[~]`

aarch64 reaches a full **concurrent, multi-process** user mode (SP2c EL0
timeshare, SP3b/SP4 `spawn`, SP6 `wait`, all `[x]`). riscv64 and x86_64
currently have only the SP5b-2 `mem_map` sibling — a **single** ring-3/U-mode
task entered through a direct `EnterUser::enter_user` (no scheduler, no
cooperative context switch). The next cross-port follow-on, **lowest-risk
first per the standing direction**, is to bring **x86_64** up to the aarch64
concurrent model, staged X1–X4 (one fully-gated chunk per landing, §0.8).

x86_64 is the lower-risk port because the machinery already exists: ring-3
entry (`rustos_arch_x86_64::userentry`), the `mem_map` producer path
(`mem_map_qemu_x86_64`), a `syscall`/`sysret` stub that already switches to a
kernel stack, an `X86_64Arch: SchedulerArch`, and an x86_64 `ContextSwitchHal`
— so `spawn_user_kthread` is largely reachable. The **riscv64** timeshare
sibling is a *separate, larger* follow-on deferred behind this arc (see the
end of this section): its `trap.s` runs the handler on the interrupted **user**
`sp` with no `sscratch` kernel-stack swap, so it needs a trap-entry redesign
before a cooperative mid-handler park can work at all.

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
  - `init_spawn_x86_64::X86_64InitSpawn` (`with_init`): builds `init`'s ring-3
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
  `ProcessSpawn` producer — `kernel/rustos-kernel/src/spawn_producer_x86_64.rs`,
  the cross-port sibling of the aarch64 `spawn_producer.rs` — is wired through
  `BootInfo::with_spawn` (in `boot::try_boot`, beside the X3a `with_init` seam)
  with the embedded `X86_64_PROGRAM_REGISTRY` (the `Shell` `rxe` `build.rs`
  already bakes for x86_64). On `init`'s `CAP_PROC_SPAWN`-gated `spawn` for
  `/Apps/Shell.app/Run`, it claims a fresh `PageTablePool` from a `.bss` reserve
  (fail-closed `NoSpace`), builds a 4 GiB-identity child PML4 with
  `new_identity_first_gib`, drives the audited `spawn_image` + `admit_process`
  (the child gets only `{CAP_CONSOLE_WRITE}`, no ambient authority), and admits
  it **Ready** — returning the PID without entering it (a true concurrent spawn).
  **Key decision:** unlike the X3a PID-1 seam (which switches `CR3` to build the
  image), the producer runs under PID 1's own `CR3` — whose
  `new_identity_first_gib` map covers the low 4 GiB identity (existing-table
  physical derefs + the `.bss` child pool + the allocator's frames) **and** the
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

- **X4 — x86_64 `wait` sibling `[ ]`.** Wire the `KernelProcessWait` producer
  on the x86_64 production pipeline (`register_child` on the spawn-admit path,
  `record_exit` in `exit`) + an x86_64 `wait` vertical mirroring
  `wait_qemu_aarch64` (a parent blocks on, reaps, and reads back a child's exit
  code).

**Done when (per chunk):** the chunk's QEMU vertical PASSes under `cargo xtask
test --qemu` **and** the whole-project gate (§5) is green; docs + host tests
land in the same change (§7 / §13).

**riscv64 concurrent user mode (deferred follow-on) `[ ]`.** After the x86_64
arc, the riscv64 spawn/wait timeshare needs `trap.s` to (1) swap to a per-task
kernel stack via `sscratch` — it currently runs the handler on the interrupted
**user** `sp`, which the cooperative `ContextSwitch::switch` would wrongly save
— and (2) save/restore `sepc`/`sstatus` in the per-task trap frame (the same
latent errata aarch64 fixed in 4c780bc, today unreachable on riscv64 because no
cooperative mid-handler park exists yet). The 144-byte frame already has 16
spare bytes for the two CSRs. Staged separately when reached.

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
