# `rustos-vl805`

Raspberry Pi 4 (BCM2711) **VL805** xHCI USB host-controller device-support
library.

Stability tier: **experimental**.

## What it is

The Pi 4's USB-A ports hang off a VIA **VL805** PCIe-to-USB3 xHCI host
controller behind the BCM2711 PCIe root complex. On boards without the SPI
EEPROM (Pi 4 rev 1.4 and later) the VL805 carries **no resident firmware**:
the `VideoCore` co-processor loads it at power-on, and a PCIe `PERST#` —
which the root-complex bring-up asserts — drops it. Only the `VideoCore` can
(re)load it, over a firmware property-channel `NOTIFY_XHCI_RESET` request.

That firmware reload is the **one** thing specific to this device, so it is
its own driver — deliberately **separate** from, and not intertwined with:

* the generic PCIe root-complex driver `lib/pcie_brcm` /
  `drivers/bus/pcie_brcm` (trains the link), and
* the generic xHCI host-controller engine `lib/usb` (brings the controller
  up and enumerates devices).

A different board may need the PCIe driver without USB at all, or an xHCI
controller that needs no firmware reload. Keeping the three separate is the
correct modular shape (`AGENTS.md` §2.2 / §2.20 / §8 / §17.4).

## Why a `lib/*` crate (the §2.20 device-support carve-out)

The firmware policy and the controller-node wiring live here as a `lib/*`
crate so two consumers share **one** definition (`AGENTS.md` §2.2):

* the autoloaded user-space VL805 bus driver `drivers/bus/usb/vl805`, which
  links the userland runtime `rustos-rt`; and
* the transitional in-kernel keyboard bring-up scaffold (`rustos-kernel`).

A `rustos-rt`-linking bin cannot share a kernel-linked `drivers/*` crate
(the userland `_start`/allocator would enter the kernel dependency graph),
so the shared logic lives in `lib/*` and both reach it without a
`drivers/*`→`drivers/*` edge (the `lib/vcmailbox` / `lib/pcie_brcm`
precedent, `AGENTS.md` §17.4).

## Supported hardware

* VIA VL805 USB 3.0 host controller (PCI `1106:3483`) on the Raspberry Pi 4
  (BCM2711), reached through the BCM2711 PCIe root complex.

## Public surface

Per `AGENTS.md` §8 the only public *function* is `register`. The firmware
policy is `reload_firmware` and `probe_firmware_revision`, composed by the
host over the board-neutral `rustos_abi::driver::mailbox::MailboxChannel`
seam; `BIND_KEYS` is the §18.3 bind table (exact PCI `1106:3483`). The
`wiring` module exposes `build_xhci_node` (publish the controller as an
`rustos_usb::XHCI_COMPATIBLE` hardware-tree node forwarding the BAR + DMA
grants) and `reload_firmware_and_publish` (the reload-then-publish
composition).

## Capabilities

Loading requires `CAP_DRV_LOAD`. The mailbox doorbell and property-buffer
access are gated **host-side** by the `MailboxChannel` implementation, and
publishing the controller node by `CAP_HW_EMIT` (`AGENTS.md` §5.4); this
crate holds no ambient authority and names no board address (`AGENTS.md`
§4 / §2.20).

## Layering

A device-specific (`AGENTS.md` §2.20 carve-out) `lib/*` crate: it knows the
VL805/BCM2711 but reaches the firmware mailbox **only** through
`MailboxChannel` — never a doorbell address, a property-buffer carve, or a
`kernel/*` dependency (`AGENTS.md` §17.4). The property-message layout lives
once in `lib/vcmailbox`; the controller `compatible` identity and DMA
working-set size live once in `lib/usb`; this crate only sequences the
policy (`AGENTS.md` §2.2).

## Limitations / testing

QEMU models no `VideoCore`, so the firmware policy is proven host-side
against the protocol-faithful `lib/vcmailbox` mock firmware, and the
controller-node wiring against `DriverHost` doubles (`AGENTS.md` §2.1 /
§2.2). The live firmware reload is the on-metal acceptance item
(`plans/PI.md` P10).
