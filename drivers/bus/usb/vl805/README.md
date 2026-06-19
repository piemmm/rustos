# `rustos-drv-bus-usb-vl805`

Raspberry Pi 4 (BCM2711) **VL805** xHCI USB host-controller device driver.

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

* the generic PCIe root-complex driver `drivers/bus/pcie_brcm` (trains the
  link), and
* the generic xHCI host-controller driver `drivers/bus/usb/xhci` (brings the
  controller up and enumerates devices).

A different board may need the PCIe driver without USB at all, or an xHCI
controller that needs no firmware reload. Keeping the three drivers separate
is the correct modular shape (`AGENTS.md` §2.2 / §2.20 / §8 / §17.4).

## Supported hardware

* VIA VL805 USB 3.0 host controller (PCI `1106:3483`) on the Raspberry Pi 4
  (BCM2711), reached through the BCM2711 PCIe root complex.

## Public surface

Per `AGENTS.md` §8 the only public *function* is `register`. The firmware
policy is `reload_firmware` and `probe_firmware_revision`, both composed by
the host over the board-neutral `rustos_abi::driver::mailbox::MailboxChannel`
seam; `BIND_KEYS` is the §18.3 bind table (exact PCI `1106:3483`).

## Capabilities

Loading requires `CAP_DRV_LOAD`. The mailbox doorbell and property-buffer
access are gated **host-side** by the `MailboxChannel` implementation
(`AGENTS.md` §5.4); this driver holds no ambient authority and names no board
address (`AGENTS.md` §4 / §2.20).

## Layering

A device-specific (`AGENTS.md` §2.20 carve-out) `drivers/*` crate: it knows
the VL805/BCM2711 but reaches the firmware mailbox **only** through
`MailboxChannel` — never a doorbell address, a property-buffer carve, or a
`kernel/*` dependency (`AGENTS.md` §17.4). The property-message layout lives
once in `lib/vcmailbox`; this driver only sequences the policy
(`AGENTS.md` §2.2).

## Limitations / testing

QEMU models no `VideoCore`, so the policy is proven host-side against the
protocol-faithful `lib/vcmailbox` mock firmware (`AGENTS.md` §2.1 / §2.2).
The live firmware reload is the on-metal acceptance item
(`plans/PI.md` Increment C).
