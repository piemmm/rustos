# `rustos-pcie-brcm` — BCM2711 PCIe root-complex bring-up

`plans/PI.md` P10 deliverable (VL805 track). Brings up the Broadcom
BCM2711 (Raspberry Pi 4) PCIe root complex so the VL805 xHCI USB host
controller behind it becomes reachable. The root complex ships out of
reset with its link **down**; this crate resets it, powers the SerDes,
programs its inbound/outbound address windows from device-tree-discovered
values, and trains the link.

It is a `lib/*` crate, not a `drivers/*` crate, even though it is
board-specific: it is the **single device's own support code** the
`AGENTS.md` §2.20 carve-out places in `lib/` (the `lib/vcmailbox`
precedent), so the in-kernel boot scaffold and the user-space
`drivers/bus/pcie_brcm` bin can both compose it without a forbidden
`drivers/*`→`drivers/*` edge (`AGENTS.md` §17.4). The crate also hosts the
user-space bus driver's whole discover-and-publish composition
(`wiring::emit_vl805_node`), so that logic is host-tested here against a
mock bus and the binary stays a thin freestanding stub.

**Stability tier:** `experimental`. The reset/SerDes/window/link state
machine is complete and host-tested; the live link training on a real Pi
4 is the remaining metal-acceptance item.

## Layered seams

The bring-up state machine (`BrcmPcieRc`) is written against two seams so
it is proven host-side (`AGENTS.md` §2.2):

- **`PcieRegs`** — controller register access. **Metal** drives it over a
  capability-gated `RegisterWindow` (`PcieRegs` is implemented for it),
  mapped from the discovered node by `wiring::open_discovered`. **Host
  tests** drive it over a register-level mock that models the root-port
  role bit and link-up after a bounded number of status polls.
- **`Delay`** — microsecond busy-delay. The link bring-up has hard timing
  requirements (SerDes stabilisation, the post-`PERST#` settle, the
  100 ms link-training window) that no register poll can
  substitute for. On metal the kernel composition supplies a
  generic-timer-backed delay; host tests pass a no-op.

There is no Pi-board QEMU vertical — QEMU models no Pi PCIe link timing
(`plans/PI.md` §0.4) — so the emulation artefact is the host tests and
metal acceptance is a documented checklist.

## Where it sits

```
brcm,bcm2711-pcie node (hwtree)
        │  wiring::open_discovered  (maps the controller window, CAP_MMIO_MAP)
        ▼
BrcmPcieRc::open  ──►  link up
        │  into_regs()  (recover the same window)
        ▼
rustos_pci::mechanism_brcm(window)  ──►  &dyn PciBus
        │
        ▼
rustos_drv_bus_usb::wiring::open_discovered  ──►  VL805 xHCI
```

This crate performs **only** the link bring-up. Configuration-space
access to downstream functions is the BCM2711 *windowed* ECAM mechanism
(`rustos_pci::mechanism_brcm`, a `lib/*` crate); enumeration, BAR sizing,
and bus mastering are the generic PCI core. As a `lib/*` crate it depends
only on `lib/abi`, so a consumer composes it with `lib/pci` and the USB
stack without any `drivers/*`→`drivers/*` edge (`AGENTS.md` §17.4).

## Supported hardware

| Device               | Board | Status                                  |
|----------------------|-------|-----------------------------------------|
| `brcm,bcm2711-pcie`  | Pi 4  | link bring-up (host-tested); metal pending |

The aarch64 `FdtDiscovery` walk emits the `brcm,bcm2711-pcie` node into
`rustos_abi::hwtree` (Bus class, translated controller/ECAM-access window
plus the inbound-DMA aperture from `dma-ranges`); the composition maps the
window and calls `wiring::open_discovered`. The outbound `ranges` MMIO
window the root complex is also programmed with is passed in `PcieWindows`
by the composition (its discovery into the hardware tree is a follow-up,
tracked in `plans/PI.md`).

## Autoload bind table

`BIND_KEYS` (one entry) declares this driver binds the `brcm,bcm2711-pcie`
device-tree `compatible` (`AGENTS.md` §18.3) — the single source of truth a
signed-manifest bind table is authored from, and the data `devmgr`
resolves the discovered PCIe-bridge node against (`PLAN.md` Stage 4.HW
item 5).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` to map the discovered controller window
  (`wiring::open_discovered`); the window is reached only through the
  host's `MmioMapper`, never a pointer the driver synthesises
  (`AGENTS.md` §4 — no ambient authority).

## Bounded / fail-closed

- Link training is bounded by `DEFAULT_LINK_POLLS` (20 × 5 ms = 100 ms);
  a link that never trains fails closed with `DriverError::DeviceFault`
  rather than spinning (`AGENTS.md` §2.1). The poll budget is a defence
  bound, not a scalable capacity (`AGENTS.md` §24.4).
- The bring-up refuses (`DeviceFault`) if the controller never reports the
  root-port role, before advertising bridge configuration (`AGENTS.md`
  §5.4).
- An inbound-viewport size outside the representable 4 KiB‥32 GiB range
  encodes to the "disabled" value rather than a wrong window.

## Test surface

`cargo test -p rustos-pcie-brcm` exercises:

- The full reset → SerDes → window → link-up sequence over the mock,
  asserting the programmed register state (PERST# released, misc-control
  bits, inbound size encoding, RC class code, ASPM advertisement, outbound
  window) and the reset-before-SerDes ordering.
- `encode_ibar_size` across the documented size ranges and the
  out-of-range/degenerate fail-closed cases.
- Fail-closed link-down (poll budget exhausted) and not-a-root-port.
- The `wiring` capability / mapper gate, and the inert-window boundary
  (the root-port check is the on-metal hand-off point).

## Metal acceptance (pending hardware)

The on-metal bring-up checklist (boot a Pi 4, observe the link train, then
enumerate the VL805 behind the bridge) is the acceptance artefact,
recorded in `plans/PI.md` P10. It requires a physical Pi 4.

## Public surface

The surface a composing host drives is the `BrcmPcieRc` bring-up engine
(constructed through `wiring::open_discovered` / `wiring::bring_up_from_node`)
and the `BIND_KEYS` match table. For the user-space bus driver the crate also
exposes the whole composition: `wiring::pcie_bringup_from_resources` (parse
the granted RC resources into the bring-up windows),
`wiring::publish_usb_function` (the host-tested post-link half — enumerate the
USB controller, assign/enable/map its BAR, translate it to CPU-physical, and
publish the node with its grant requests), and `wiring::emit_vl805_node` (link
training + `publish_usb_function`). `register` is the thin `AGENTS.md` §8
driver entry the kernel's §18.6 bootstrap-floor catalogue invokes to gate
loading (`CAP_DRV_LOAD`); it touches no hardware. The host never reaches
into `BrcmPcieRc` beyond recovering the brought-up register window
(`into_regs`).
