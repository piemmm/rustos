# `tairix-drv-bus-pcie-brcm` — BCM2711 PCIe root-complex bus driver

Autoloaded **user-space bus driver** for the Broadcom BCM2711 (Raspberry
Pi 4) PCIe root complex. It binds the discovered `brcm,bcm2711-pcie` node,
trains the root-complex link, enumerates the single USB host controller (the
VL805) behind the bridge, assigns and enables that controller's register BAR,
and publishes it into the live hardware tree as a bindable xHCI node — so
`devmgr` autoloads the next driver in the chain (`drivers/bus/usb/vl805`)
against it (`plans/PI.md` P10 D5b.2b).

This is the "drivers in user space" steady state (`AGENTS.md` §4): the Pi 4
PCIe bring-up is no longer an in-kernel scaffold. The §18.6 bootstrap floor
stays storage-only; this driver is discovered and spawned by `devmgr` like any
other driver.

## Supported hardware

- BCM2711 PCIe root complex (Raspberry Pi 4 / Pi 400). Binds by the
  device-tree `compatible` string `brcm,bcm2711-pcie` (the crate's
  `tairix_drv_bus_pcie_brcm::BIND_KEYS`).

The controller register window, the inbound-DMA aperture, and the outbound
MMIO window are **discovered** values threaded from the matched
hardware-tree node's resource grants (`AGENTS.md` §2.20 / §18.1 / §18.3); the
driver names no board address.

## What it publishes

It emits one child `HwNode` for the USB host controller behind the bridge,
carrying exactly two device-resource grant *requests* and no more
(`AGENTS.md` §4 / §18.3):

- an `Mmio` window of the controller's assigned register BAR, resolved to its
  **CPU-physical** address through the discovered outbound window — so it lies
  inside the bridge's outbound `BusWindow` grant and the kernel's
  grant-coverage check admits it (`HwResource::covers`); and
- a `Dma` constraint declaring the device-visible inbound aperture the
  matched driver's DMA bank verifies every allocation against (each chunk it
  grows must lie wholly below the aperture top, fail closed).

The node's identity (id/parent) is **kernel-assigned** on publish; the driver
does not name it (`AGENTS.md` §4 / §18.1). The composition lives — and is
host-tested against a mock bus — in this crate's own `lib` target
(`crate::wiring::emit_vl805_node` / `crate::wiring::publish_usb_function`,
`src/lib.rs`); `src/main.rs` is the thin freestanding `Run` binary that links
it.

## Crate shape — device logic is co-located here, not in `lib/*`

This crate has two targets: a host-testable **`lib` target** (`src/lib.rs`,
`src/regs.rs`, `src/wiring.rs`) holding the BCM2711 PCIe bring-up engine
(`BrcmPcieRc`), the §18.3 `BIND_KEYS`, and the `wiring`; and the **`Run`
binary** (`src/main.rs`) that builds the rt-backed host and drives it. The
device logic lives **in the driver**, not a `lib/*` device-support crate,
because the §2.20 carve-out only permits the latter when a charter-legal
*non-driver* consumer (a §18.6 bootstrap-floor path, or a driver of a
different class) shares it — and PCIe root-complex bring-up sits above the
bootstrap floor (the kernel floor is the storage path only), so its only
consumer is this crate's own `Run` binary (`AGENTS.md` §2.22 / §2.2 / §2.14).

## Required capabilities

- `CAP_MMIO_MAP` — map the discovered controller register window (link
  training) and probe the controller's BAR.
- `CAP_HW_EMIT` — publish the enumerated child node into the live hardware
  tree.

It carves no DMA itself and runs in user space (no `CAP_MEM_DMA`, no
`CAP_DRV_KERNEL`).

## Runtime / unload

It stays resident after publishing the node, holding the trained root complex
for the life of the system; it parks in a yield loop (`AGENTS.md` §2.1). A
bring-up failure exits with a reserved fail-closed code, leaving the bus
unbrought-up rather than wedged (`AGENTS.md` §2.9).

## Install

The signed bundle is installed into the image's `/System/Drivers/` store
(`tools/xtask` builds and signs it; `tools/mkimage` plants it). It is the head
of the autoloaded four-bundle Pi 4 USB chain (`pcie_brcm` → `vcmailbox` →
`vl805` → `usb_kbd`); there is no in-kernel keyboard scaffold (`AGENTS.md`
§18.5).

## Limitations

- Single-board: BCM2711 only. Other PCIe-bearing SoCs are out of scope here.
- Verifiable on metal only: QEMU `virt` models no Pi PCIe link timing
  (`plans/PI.md` §0.4), so there is no QEMU integration vertical; the
  enumerate → assign-BAR → publish composition and its fail-closed paths are
  host-tested in this crate's `lib` target, and the live link training is the
  on-metal acceptance item.
- The BAR is assigned **before** the VL805 firmware reload (the next driver,
  `drivers/bus/usb/vl805`), whereas the proven in-kernel order reloaded
  firmware first; BAR assignment is PCI-config-level (independent of the xHCI
  firmware), so it is expected safe, but it is a metal-gated acceptance point
  (`plans/PI.md`).

## Stability

`experimental` — part of the in-progress P10 user-space driver migration.
