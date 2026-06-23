# `rustos-drv-bus-usb-vl805`

Autoloaded **user-space** Raspberry Pi 4 (BCM2711) **VL805** USB bus driver.

Stability tier: **experimental**.

## What it is

The `Run` entry-point binary of the VL805 USB bus-driver bundle, installed
under `/System/Drivers/` and autoloaded into user space by `devmgr` when the
VL805 PCI node is discovered (`AGENTS.md` §18; `plans/PI.md` P10 D5c).

The Pi 4's USB-A ports hang off a VIA **VL805** PCIe-to-USB3 xHCI host
controller behind the BCM2711 PCIe root complex. On boards without the SPI
EEPROM (Pi 4 rev 1.4 and later) the VL805 carries no resident firmware: the
`VideoCore` co-processor loads it at power-on, and the PCIe `PERST#` the
root-complex bring-up asserts drops it. This driver is the device-specific
link in the chain:

1. the PCIe root-complex bus driver (`drivers/bus/pcie_brcm`) trains the
   link, enumerates the VL805, assigns its register BAR, and publishes it as
   a VL805 PCI node (`node A`) carrying that BAR (CPU-physical) and the
   inbound-DMA constraint as grant requests;
2. **this driver** binds node A, reloads the VL805 firmware over the
   `VideoCore` mailbox IPC, and publishes the controller as `node B` — an
   `usb,xhci` node **forwarding** node A's BAR + DMA grants; and
3. the controller driver (`drivers/input/usb_kbd`) binds node B, maps the
   BAR, brings the controller up, and pumps input.

Firmware-before-bring-up holds **by construction**: node B does not exist
until this driver runs the reload.

## Capabilities — least privilege

This driver holds **only** `CAP_MAILBOX` (reload the firmware over the
`VideoCore` mailbox service) and `CAP_HW_EMIT` (publish node B). It is
deliberately **not** granted `CAP_MMIO_MAP` / `CAP_MEM_DMA`: it forwards the
BAR + DMA grants to the controller driver without ever mapping them itself
(`AGENTS.md` §4 — no ambient authority). Every trap is re-checked kernel-side
(`AGENTS.md` §5.4).

## Layering & purity

A **pure-Rust** program (`AGENTS.md` §1): it links the userland runtime
`rustos-rt` (never the C ABI, §16.4) and depends only on `lib/*` crates, so
the §17.4 layering holds (no `drivers/*`→`drivers/*` edge). The
reload-and-publish composition and the firmware policy live in `lib/vl805`
(`wiring`, host-tested against `DriverHost` doubles); this binary is the thin
freestanding wiring that builds the rt-backed host and drives it. It names no
board address and maps nothing, so it stays platform-neutral (`coherency =
None`, §2.20). On the host it is an inert stub.

## Limitations / testing

QEMU models no `VideoCore` mailbox or Pi USB timing (`AGENTS.md` §0.4), so
the composition and its fail-closed paths are proven by the `lib/vl805` host
tests; the live reload → publish chain is the on-metal acceptance item
(`plans/PI.md` P10).
