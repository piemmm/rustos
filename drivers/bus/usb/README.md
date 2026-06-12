# `rustos-drv-bus-usb` — xHCI USB host-controller driver

`plans/PI.md` P10 deliverable (host-provable slice). Carries the xHCI
protocol layers and the HID enumeration engine the Pi 4's USB
bring-up builds on:

- `regs` — the capability / operational / runtime (interrupter) /
  doorbell register vocabulary (xHCI 1.2 §5), with only the registers
  the driver touches defined.
- `trb` — the 16-byte TRB vocabulary (§6.4): a fail-closed `TrbType`
  and `CompletionCode` subset, event-field decode (slot, endpoint ID,
  transfer residual), and the on-ring byte conversion.
- `ring` — the memory-free ring state machines (§4.9): `ProducerRing`
  (cycle-bit stamping, Link-TRB wrap with Toggle Cycle, full-ring
  refusal, completion retirement) returning `PushOutcome`s the memory
  owner publishes, and the `EventRingCursor` consumer (cycle-ownership
  check, wrap toggling) over caller-provided snapshots.
- `Xhci` — the §4.2 bring-up over the `XhciHost` register seam:
  `open` validates the capability block (absent/broken controllers
  fail closed), waits Controller-Not-Ready, halts a running
  controller, and issues the Host Controller Reset; `start` programs
  `CONFIG`/`DCBAAP`/`CRCR` and interrupter 0's event ring
  (`ERSTSZ`/`ERSTBA`/`ERDP` over `RTSOFF`) and runs the controller;
  plus `ack_event`, the RW1C-safe `reset_port`, bounds-checked
  `PORTSC` decode, and doorbell rings.
- `device` — the single-device HID enumeration engine over the
  `DmaRegion` seam (`DmaSlab` in production, a shared buffer in
  tests): a 64-byte-aligned layout of every device-shared structure,
  Enable Slot / Address Device / Configure Endpoint command flow,
  control transfers (`GET_DESCRIPTOR(device)` decoded fail-closed,
  `SET_CONFIGURATION(1)`, `SET_PROTOCOL(boot)`), a primed
  interrupt-IN transfer ring, and the
  `rustos_abi::driver::input::ReportSource` impl that feeds the
  `drivers/input/usb_hid` decoders.
- `wiring` — the driver-host composition `open_discovered(host, bus,
  dma_aperture_top)`: given the PCI bus built over the discovered
  `brcm,bcm2711-pcie` ECAM window (`rustos_drv_bus_pci::mechanism_ecam`,
  reached through the `lib/abi` `PciBus` seam so this crate never names
  the PCI crate, `AGENTS.md` §17.4), it enumerates for the USB-class
  function, carves the device-shared DMA region and verifies it lies
  below the discovered inbound-DMA aperture (fail-closed `OutOfRange`,
  §5.4), enables bus mastering, maps BAR0 under `CAP_MMIO_MAP`, and
  brings the controller up via `Xhci::open` + `UsbDevice::start`.

## Supported hardware

| Platform | Controller                   | Status                              |
|----------|------------------------------|-------------------------------------|
| Pi 4     | VL805 PCIe xHCI (USB-A ports) | protocol layers + HID enumeration + `PciBus` BAR/DMA wiring host-proven; live controller bring-up is the metal acceptance item |

The register window arrives through the hardware tree (PCI BAR
assignment under `CAP_MMIO_MAP`) — never a compiled-in base
(`AGENTS.md` §18.1). `wiring::open_discovered` composes the discovered
ECAM window, the `PciBus` BAR/bus-master seam, and the host DMA
facility; the live controller bring-up over a real BAR is the remaining
metal acceptance item. QEMU models no Pi USB timing, so the emulation
artefact is the host test suite and metal acceptance stays a checklist
(`plans/PI.md` §0.4 watch-out).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` to map the discovered register window (checked by the
  wiring that mints the `RegisterWindow` the `XhciHost` seam is
  implemented for).

The driver runs in user space; it does **not** request
`CAP_DRV_KERNEL` (`AGENTS.md` §4 / §8).

## Limitations

- One enumerated device per engine: `UsbDevice` drives a single HID
  device's slot, control pipe, and interrupt-IN endpoint. Multi-device
  topologies (hubs) are out of scope for the boot-input bring-up.
- The event ring is a single segment serviced by polling; interrupter
  interrupts (MSI-X) are not enabled — `next_report` is a non-blocking
  poll, matching the `Input::poll` shape above it.

## Test surface

`cargo test -p rustos-drv-bus-usb` exercises, against a register-level
mock controller:

- `register` capability gate.
- Bring-up: capability-block parse, ready-wait, halt-then-reset of a
  running controller, absent (all-ones) and implausible capability
  blocks rejected, stuck ready/reset failing closed within the poll
  budget.
- `PORTSC` decode and port bounds; doorbell index/target bounds and
  write offsets.
- TRB type / completion-code round-trips failing closed on unknown
  values; byte-image round-trips; transfer-event field decode.
- Producer ring: cycle stamping, reported TRB addresses, Link-TRB
  publication and cycle toggle across the wrap, full-ring `Busy`,
  retirement underflow.
- Event cursor: cycle-ownership consumption, wrap-and-toggle, stale
  TRBs ignored, wrong-segment rejection.
- DMA programming: `CONFIG`/`DCBAAP`/`CRCR`/interrupter registers
  captured by the mock, run/start, misaligned or undersized regions
  refused.
- Enumeration, against the register-level mock plus an in-memory ring
  model sharing one buffer: the full chain (port reset when disabled,
  Enable Slot, Address Device, descriptor fetch, Configure Endpoint,
  `SET_CONFIGURATION`, `SET_PROTOCOL(boot)`), empty-port and
  double-enumeration refusals, stalled class requests failing closed.
- The report path: reports (full and short) polled through
  `ReportSource`, retire/re-arm across the Link-TRB wrap, forged
  residuals failing closed, and a `BootKeyboard` from
  `drivers/input/usb_hid` decoding key events end-to-end over the mock
  controller.
- `wiring::open_discovered`, against mock `PciBus` / `MmioMapper` /
  DMA host: the `CAP_MMIO_MAP`, mapper-absent, and DMA-host-absent
  fail-closed paths; a bus with no USB-class function refused
  (`NotFound`); a DMA carve above the aperture refused (`OutOfRange`)
  before any hardware is touched; a DMA allocation failure propagated;
  and the all-valid path enabling bus mastering and reaching the
  controller hand-off (the inert mock window faults, the on-metal
  boundary).
