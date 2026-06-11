# `rustos-drv-bus-usb` — xHCI USB host-controller driver

`plans/PI.md` P10 deliverable (host-provable slice). Carries the xHCI
protocol layers the Pi 4's USB bring-up builds on:

- `regs` — the capability / operational / doorbell register vocabulary
  (xHCI 1.2 §5), with only the registers the driver touches defined.
- `trb` — the 16-byte TRB vocabulary (§6.4): a fail-closed `TrbType`
  and `CompletionCode` subset plus event-field decode.
- `ring` — the ring state machines (§4.9): `ProducerRing` (cycle-bit
  stamping, Link-TRB wrap with Toggle Cycle, full-ring refusal,
  completion retirement) and the `EventRingCursor` consumer
  (cycle-ownership check, wrap toggling) over caller-provided TRB
  memory.
- `Xhci::open` — the §4.2 bring-up prologue over the `XhciHost`
  register seam: validate the capability block (absent/broken
  controllers fail closed), wait Controller-Not-Ready, halt a running
  controller, Host Controller Reset; plus bounds-checked `PORTSC`
  decode and doorbell rings.

## Supported hardware

| Platform | Controller                   | Status                              |
|----------|------------------------------|-------------------------------------|
| Pi 4     | VL805 PCIe xHCI (USB-A ports) | protocol layers host-proven; PCI BAR wiring + enumeration pending |

The register window arrives through the hardware tree (PCI BAR
assignment under `CAP_MMIO_MAP`) — never a compiled-in base
(`AGENTS.md` §18.1). Device enumeration (DCBAAP/CRCR programming,
slots, control transfers, HID interrupt endpoints feeding
`drivers/input/usb_hid`) is the remaining P10 work and lands in
follow-up increments. QEMU models no Pi USB timing, so the emulation
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

- Bring-up stops at the halted, freshly reset controller: starting it
  needs the DMA memory (device context array, command ring) the
  enumeration increment brings.
- The event-ring consumer models a single segment; the runtime
  (interrupter) register block is located (`RTSOFF`) but not yet
  programmed.

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
  values.
- Producer ring: cycle stamping, reported TRB addresses, Link-TRB
  publication and cycle toggle across the wrap, full-ring `Busy`,
  retirement underflow.
- Event cursor: cycle-ownership consumption, wrap-and-toggle, stale
  TRBs ignored, wrong-segment rejection.
