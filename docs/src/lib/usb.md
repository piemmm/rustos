# `rustos-usb`

`lib/usb` is the **bus-agnostic xHCI USB host-controller protocol**: the
host-provable, controller-agnostic layers of an xHCI stack, with no PCI or
board coupling. It is the USB analogue of `lib/virtio` — the protocol lives in
`lib/` so more than one crate can consume it (`AGENTS.md` §2.2 / §6 / §17.4),
and it depends only on `lib/abi`, so it builds for every Tier-1 target and is
identical on `aarch64`, `x86_64`, and `riscv64` (the USB protocol does not vary
by architecture).

## Why it exists

The USB host-controller protocol used to live inside `drivers/bus/usb`. But the
§17.4 layering forbids a `drivers/*` (or `userland/*`) crate from depending on
another `drivers/*` crate, so an arch-neutral user-space keyboard driver could
not reuse the xHCI engine and the HID enumeration path while they sat in the bus
driver. Moving the protocol into `lib/usb` lets both the concrete
host-controller driver (`drivers/bus/usb`, which adds the PCI
discovery/BAR/DMA wiring and the §8 `register` entry) and the keyboard driver
build on the *same* engine without depending on each other — exactly the split
`lib/virtio` ↔ `drivers/bus/virtio` already uses.

## What it provides

- `XhciHost` — the register-access seam every controller access goes through.
  On metal it is a capability-gated `RegisterWindow` whose base the hardware
  tree discovered (PCI BAR assignment, never a compiled-in constant, §18.1); in
  host tests it is a register-level mock controller.
- `Xhci` — the controller engine. `open` validates the capability block and
  runs the xHCI 1.2 §4.2 prologue (halt, clear latched status, Host Controller
  Reset, wait ready); `start` programs the DMA structures (`DCBAAP`, command
  ring, interrupter-0 event ring) and runs the controller; `reset_port` /
  `set_port_power` / `ring_doorbell` / `ack_event` drive the root hub and rings.
- `device::UsbDevice` — the single-device HID enumeration engine: Enable Slot →
  Address Device → `GET_DESCRIPTOR` → `SET_CONFIGURATION` → `SET_PROTOCOL(boot)`
  → Configure Endpoint → a primed interrupt-IN ring, then the
  `rustos_abi::driver::input::ReportSource` seam so the `drivers/input/usb_hid`
  decoders read reports straight off the transfer ring. The interrupt-IN
  endpoint's DCI, packet size, and interval are read from the device's endpoint
  descriptor (never hard-coded). `enumerate_boot_keyboard` is the arch-neutral
  bring-up orchestration a keyboard driver runs once: it enumerates the first
  connected root-hub port and, when that device is itself a hub (the Pi 4B's
  onboard hub), powers the hub's ports, finds the connected one, resets it
  (settle windows supplied by the `rustos_abi::Delay` seam), and addresses the
  device behind it on a second slot — so the keyboard is discovered, never a
  guessed port, with one definition shared by every consumer (§2.2 / §18).
- `regs` / `trb` / `ring` — the register, TRB, and ring-state vocabularies; the
  ring state machines (`ProducerRing`, `EventRingCursor`) hold no memory of
  their own, so the owner publishes every write through the `device::DmaRegion`
  seam.
- `XHCI_DMA_BYTES` — the bytes a host carves for one controller's device-shared
  DMA structures (rings, contexts, report buffers, scratchpad), sized for the
  VL805's 31-page scratchpad worst case. It lives here, beside the engine that
  lays the region out, so every host that carves it — the PCI bus driver's
  wiring (`drivers/bus/usb`) and the arch-neutral keyboard driver
  (`drivers/input/usb_hid`) — shares one definition (§2.2).

## Design

- `no_std`, `#![forbid(unsafe_op_in_unsafe_fn)]`, `lib/abi`-only.
- Every controller and DMA access is mediated by the `XhciHost` /
  `device::DmaRegion` seams, so the bring-up, enumeration, and ring state
  machines are proven host-side against a register-level mock plus an in-memory
  ring/DMA model (§2.2); the doorbell below them is the on-metal acceptance
  item (no QEMU `raspi*` USB vertical exists, §0.4).
- Fail-closed (§2.9): an implausible capability block, an out-of-range port or
  doorbell target, a malformed descriptor, or an exhausted poll budget is a
  typed `DriverError`, never a panic or an unbounded spin (§2.1).
- The crate holds **no** capability of its own — authority is the consuming
  driver's (`CAP_MMIO_MAP` for the register window, `CAP_MEM_DMA` for the DMA
  carve), checked in the wiring that mints them.

## Stability

Tier: `experimental`.
