# rustos-usb

Bus-agnostic xHCI USB host-controller protocol for RustOS (`lib/usb`,
`AGENTS.md` §6 / §2.2 — `plans/PI.md` P10).

The USB protocol is identical on every architecture, so the host-provable,
controller-agnostic xHCI layers live here once, in `lib/`, rather than inside
any one driver. This is the USB analogue of `lib/virtio`: a concrete
host-controller driver (`drivers/bus/usb`, which adds the PCI
discovery/BAR/DMA wiring and the §8 `register` entry) and an arch-neutral
user-space keyboard driver both consume this crate without depending on each
other (§17.4 — `drivers/* → lib/*` only).

## API

- `XhciHost` — the register-access seam every controller access goes through
  (metal: a capability-gated `RegisterWindow`; tests: a register-level mock).
- `Xhci` — the controller engine: `open` runs the §4.2 prologue (halt, reset,
  wait ready) and parses the capability block; `start` programs the DMA
  structures and runs the controller; `reset_port` / `set_port_power` /
  `ring_doorbell` / `ack_event` drive the root hub and rings.
- `device::UsbDevice` — the single-device HID enumeration engine (Enable Slot →
  Address Device → descriptors → `SET_PROTOCOL(boot)` → Configure Endpoint →
  primed interrupt-IN ring), implementing the `rustos_abi::driver::input`
  `ReportSource` seam so the `drivers/input/usb_hid` decoders read reports
  straight off the transfer ring.
- `regs` / `trb` / `ring` — the register, TRB, and ring-state vocabularies.
- `XHCI_DMA_BYTES` — the device-shared DMA carve size for one controller
  (rings, contexts, report buffers, scratchpad), shared by every host that
  carves a controller's DMA region (§2.2).

## Design

- `no_std`, `#![forbid(unsafe_op_in_unsafe_fn)]`, depends only on `lib/abi`, so
  it builds for every Tier-1 target.
- Every access is mediated by the `XhciHost` / `device::DmaRegion` seams, so
  the bring-up, enumeration, and ring state machines are proven host-side
  against a register-level mock plus an in-memory ring/DMA model (§2.2); the
  doorbell below them is the on-metal acceptance item.
- Fail-closed (§2.9): an implausible capability block, an out-of-range port or
  doorbell target, a malformed descriptor, or an exhausted poll budget is a
  typed `DriverError`, never a panic or an unbounded spin (§2.1).
- The crate holds **no** capability of its own — authority is the consuming
  driver's (`CAP_MMIO_MAP` for the register window, `CAP_MEM_DMA` for the DMA
  carve), checked in the wiring that mints them.

## Stability

Tier: `experimental`.
