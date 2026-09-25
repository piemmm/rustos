# tairix-usb

Bus-agnostic xHCI USB host-controller protocol for TAIRiX (`lib/usb`,
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
- `device::UsbDevice` — the multi-device enumeration engine (Enable Slot →
  Address Device → descriptors → Configure Endpoint → `SET_CONFIGURATION` →
  HID protocol set-up), implementing the `tairix_abi::driver::input`
  `ReportSource` seam so the `lib/hid` decoders (composed by the `usb_kbd`/`usb_mouse` class drivers) read reports
  straight off the transfer ring. Each slot's output context, EP0 ring, and
  control data buffer live in its own region, so a device that answers a
  timed-out control transfer late writes only that region. A control transfer
  that does not complete — refused, ended by any error, or left unanswered past
  its wait — has its endpoint taken back before the failure returns (Reset
  Endpoint from a halt, Stop Endpoint from a TD still armed, then Set TR Dequeue
  Pointer onto a rebuilt ring), so the next transfer runs. Every slot the
  engine gives up — a detach, a failed attach, or a device with no interface it
  serves (refused `Unsupported` before anything is configured) — is disabled
  before its region goes. A re-driven enumeration resets the port first: a
  device that took its address answers no fresh slot's `SET_ADDRESS` until a
  reset returns it to Default state. `device_identity` names each served
  interface by its bus position (root port + Route String), its descriptor
  identity, and — for a device serving a mass-storage interface — its serial
  number (`device::DeviceIdentity`), reproduced by `reset_and_reenumerate` for a
  device still where it was. `DeviceIdentity::recognises` is what a host
  driver keeps a node on across the reset, and it never recognises a storage
  interface without a serial; within one enumeration an identity is its
  device's exactly when equal. The serial is read in the first language the
  device lists, each string descriptor at exactly its advertised length; a
  refused, faulted, or malformed read leaves the identity without one. A
  node's device address (`describe_device`) is the device's bus position, which
  the reset keeps where the slot it reassigns would not.
- `regs` / `trb` / `ring` — the register, TRB, and ring-state vocabularies.
- `SlabBank` — the production `device::DmaBank`: a growable bank of owned
  DMA chunks minted from the host's `DmaHost` seam, aperture-checked per
  allocation and freed on release, so per-device memory is allocated on
  attach and returned on detach — never a fixed carve. A chunk the controller
  may still reach — a slot whose Disable Slot went unconfirmed, which also
  keeps its DCBAA entry — is withheld instead, and returned when that command's
  late confirmation arrives or after a confirmed controller reset; a
  `UsbDevice` resets its controller whenever it is dropped, keeping every chunk
  if the controller will not reset.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_op_in_unsafe_fn)]`, depends only on
  `lib/*` crates (`lib/abi`, `lib/dma-barrier`, `lib/hid`, `lib/inline`), so it
  builds for every Tier-1 target. The enumeration engine's
  tables and DMA chunks grow with the devices actually served, through the
  fallible allocation paths (exhaustion is a typed error, never a panic):
  the only concurrency bounds are the controller's reported slot count and
  genuine memory exhaustion, exactly as on other hosts — never a
  compile-time budget.
- Every access is mediated by the `XhciHost` / `device::DmaBank` seams, so
  the bring-up, enumeration, and ring state machines are proven host-side
  against a register-level mock plus an in-memory ring/DMA model (§2.2); the
  doorbell below them is the on-metal acceptance item.
- Synchronous completion waits **park** on the caller-supplied
  `device::EventWait` seam (on metal: the HCD's `irq_wait` on the
  controller's bound interrupt line, which the caller binds before
  `UsbDevice::start` — start enables the completion interrupter itself) and
  are bounded by wall-clock budgets (the USB 2.0 §9.2.6 request ceiling for
  a completion, the power-on-good + attach-debounce window for the boot
  connect scan). Only the brief register handshakes (`Xhci` open/start/
  reset readiness) keep the bounded iteration poll budget.
- Fail-closed (§2.9): an implausible capability block, an out-of-range port or
  doorbell target, a malformed descriptor, or an exhausted wait budget is a
  typed `DriverError`, never a panic or an unbounded spin (§2.1).
- Fuzzed: `tests/fuzz_descriptors.rs` (registered with `cargo xtask fuzz`)
  holds the pure descriptor decoders — `DeviceDescriptor::decode`,
  `InterfaceInfo::decode_all`, `HubDescriptor::decode`, `StringHeader`,
  `first_langid`, `SerialNumber::decode` — to a naive model of what they may
  accept. The transfer path validates through those same decoders only.
- The crate holds **no** capability of its own — authority is the consuming
  driver's (`CAP_MMIO_MAP` for the register window, `CAP_MEM_DMA` for the DMA
  carve), checked in the wiring that mints them.

## Stability

Tier: `experimental`.
