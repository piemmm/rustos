# `rustos-drv-bus-usb` — xHCI USB host-controller driver

`plans/PI.md` P10 deliverable (host-provable slice). This is the
concrete, loadable xHCI host-controller **driver**: the §8 `register`
entry, the §18.3 `BIND_KEYS` bind table, and the PCI discovery / BAR /
DMA `wiring` that brings a discovered controller online.

The bus-agnostic xHCI **protocol** (the `XhciHost` register seam, the
`Xhci` controller engine, the TRB / ring vocabulary, and the
single-device HID `UsbDevice` enumeration engine) lives in the
[`rustos-usb`](../../../lib/usb) crate (`lib/usb`), so this driver and an
arch-neutral user-space keyboard driver can both build on the same engine
without depending on each other (`AGENTS.md` §17.4 — `drivers/* → lib/*`
only; the USB analogue of `lib/virtio` ↔ `drivers/bus/virtio`). See
`docs/src/lib/usb.md` for the protocol surface.

What lives here:

- `register` — the §8 driver entry; `CAP_DRV_LOAD` gated.
- `BIND_KEYS` — the §18.3 bind table (one class-wildcard key).
- `wiring` — the driver-host composition `open_discovered(host, bus,
  dma_aperture_top, outbound_window)`: given the PCI bus built over the
  discovered `brcm,bcm2711-pcie` ECAM window
  (`rustos_pci::mechanism_ecam`, reached through the `lib/abi`
  `PciBus` seam so this crate never names the PCI crate, `AGENTS.md`
  §17.4), it enumerates for the USB-class function, carves the
  device-shared DMA region and verifies it lies below the discovered
  inbound-DMA aperture (fail-closed `OutOfRange`, §5.4), assigns and maps
  BAR0 under `CAP_MMIO_MAP`, enables bus mastering, and brings the
  controller up via `rustos_usb::Xhci::open` + `UsbDevice::start`.

## Supported hardware

| Platform | Controller                   | Status                              |
|----------|------------------------------|-------------------------------------|
| Pi 4     | VL805 PCIe xHCI (USB-A ports) | protocol (`lib/usb`) + `PciBus` BAR/DMA wiring host-proven; live controller bring-up is the metal acceptance item |

The register window arrives through the hardware tree (PCI BAR
assignment under `CAP_MMIO_MAP`) — never a compiled-in base
(`AGENTS.md` §18.1). `wiring::open_discovered` composes the discovered
ECAM window, the `PciBus` BAR/bus-master seam, and the host DMA
facility; the live controller bring-up over a real BAR is the remaining
metal acceptance item. QEMU models no Pi USB timing, so the emulation
artefact is the host test suite and metal acceptance stays a checklist
(`plans/PI.md` §0.4 watch-out).

## Autoload bind table

`BIND_KEYS` (one entry) declares this driver binds **any** xHCI host by
PCI class alone — class `0x0C0330` with a vendor/device wildcard
(`HwMatchKey::matches`), so the Pi 4's VL805 and any other xHCI
controller autoload it without the device id being hard-coded
(`AGENTS.md` §2.2 / §18.3). It is the single source of truth a
signed-manifest bind table is authored from; `devmgr` resolves the
discovered VL805 node against it once the bus enumeration emits that
node into the tree (`PLAN.md` Stage 4.HW item 5).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` to map the discovered register window (checked by the
  wiring that mints the `RegisterWindow` the `XhciHost` seam is
  implemented for).

The driver runs in user space; it does **not** request
`CAP_DRV_KERNEL` (`AGENTS.md` §4 / §8).

## Limitations

- One enumerated device per engine: `rustos_usb::device::UsbDevice`
  drives a single HID device's slot, control pipe, and interrupt-IN
  endpoint. Multi-device topologies (hubs) are out of scope for the
  boot-input bring-up.
- The event ring is a single segment serviced by polling; interrupter
  interrupts (MSI-X) are not enabled — `next_report` is a non-blocking
  poll, matching the `Input::poll` shape above it.

## Test surface

The xHCI protocol layers (bring-up, port/doorbell decode, TRB/ring state
machines, DMA programming, the full HID enumeration chain, and the
`ReportSource` report path including a `drivers/input/usb_hid`
`BootKeyboard` decoding key events end-to-end over the mock controller)
are tested in `lib/usb` (`cargo test -p rustos-usb`).

`cargo test -p rustos-drv-bus-usb` exercises the pieces that live here:

- the `register` `CAP_DRV_LOAD` gate and the `BIND_KEYS` class match
  (and the EHCI prog-if non-match);
- `wiring::open_discovered`, against mock `PciBus` / `MmioMapper` / DMA
  host: the `CAP_MMIO_MAP`, mapper-absent, and DMA-host-absent
  fail-closed paths; a bus with no USB-class function refused
  (`NotFound`); a DMA carve above the aperture refused (`OutOfRange`)
  before any hardware is touched; a DMA allocation failure propagated;
  and the all-valid path enabling bus mastering and reaching the
  controller hand-off (the inert mock window faults, the on-metal
  boundary).
