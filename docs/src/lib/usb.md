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
- `device::UsbDevice` — the multi-device enumeration engine: per device,
  Enable Slot → Address Device → `GET_DESCRIPTOR` → `SET_CONFIGURATION` →
  `SET_PROTOCOL(boot)` → Configure Endpoint, into a table of up to
  `MAX_DEVICES` concurrently served devices, each with its own layout region
  (EP0 / interrupt / bulk rings and buffers). `next_report(index, …)` arms one
  interrupt-IN transfer for the class URB device `index` is currently serving,
  and `engine_for(index)` is the per-device `UrbEngine` view the HCD's URB
  service drives — one interface's transfers can never reach another
  device's endpoints. Endpoint DCIs, packet sizes, and intervals are read
  from each device's descriptors (never hard-coded).
  `bring_up` is the arch-neutral bring-up orchestration the host-controller
  driver runs once: it enumerates the first connected root-hub port and, when
  that device is itself a hub (the Pi 4B's onboard hub), powers the hub's
  ports and enumerates **every** connected downstream port (settle windows
  supplied by the `rustos_abi::Delay` seam) — a keyboard and a storage stick
  plugged in together are both served, neither displacing the other, and a
  port whose device fails enumeration is skipped with its slot released,
  never allowed to cost the other devices their service. A device absent at
  bring-up is a first-class state, not a failure: the controller comes up
  with the first-connect watch armed (the onboard hub's status-change
  endpoint, or the root port), so a cold boot with nothing plugged in works
  and each device autoloads when plugged in.
- `regs` / `trb` / `ring` — the register, TRB, and ring-state vocabularies; the
  ring state machines (`ProducerRing`, `EventRingCursor`) hold no memory of
  their own, so the owner publishes every write through the `device::DmaRegion`
  seam.
- `XHCI_DMA_BYTES` — the bytes a host carves for one controller's device-shared
  DMA structures (rings, contexts, report buffers, bulk staging, scratchpad),
  sized for the VL805's 31-page scratchpad worst case plus the bulk
  rings/staging. It lives here, beside the engine that
  lays the region out, so every host that carves it — the PCI bus driver's
  wiring (`drivers/bus/usb`) and the arch-neutral HID class drivers
  (`drivers/input/usb_kbd`, `drivers/input/usb_mouse`) — shares one definition (§2.2).
- `XHCI_COMPATIBLE` — the `compatible` identity (`usb,xhci`) a discovered xHCI
  controller node carries (§18.1). An xHCI-protocol identity (not a board or
  vendor name), so it lives here as the single definition the emitting bus
  driver (`drivers/bus/usb/vl805`, which publishes the controller node under
  it) and the binding controller driver (`drivers/input/usb_kbd`'s
  `KEYBOARD_BIND_KEYS`) share (§2.2 / §2.20).

- `transport` — the **bus-agnostic URB transport seam** the modular USB stack
  (`plans/USB.md`) is built on. The wire contract is `rustos_abi::usb_urb`: a
  `UrbRequest` (endpoint, transfer type, direction, shared-buffer handle,
  length, control SETUP) and a status-framed completion (bytes transferred, or
  an in-band `Errno`). `transport` adds the two ends both sides share:
  - `UrbEngine` — the controller-side operation seam the HCD's live engine
    performs (`UsbDevice` implements it: `control_in` over the EP0 control
    transfer — targeting the enumerated *device*, switching a hub-downstream
    device's EP0 ring active for the transfer — `control_no_data` over the
    same path for a SETUP-only class request (the BOT Mass Storage Reset,
    `plans/DEVICES.md` D2), `interrupt_in` over the `ReportSource` report
    poll, and `bulk_in` / `bulk_out` over the bulk endpoint pair a
    mass-storage interface configures at enumeration, `plans/DEVICES.md`
    D1).
  - `drive_urb` — the controller-side server transformation: decode a URB,
    validate it fail-closed against the interface (control ⇒ endpoint 0,
    served as IN or the zero-length no-data OUT; interrupt/bulk ⇒ a device
    endpoint; an oversize length, a control-OUT *data stage*, or a
    malformed frame is refused **before** the engine is touched), drive the
    engine over the shared buffer, and frame the
    completion in band. A not-yet-arrived interrupt-IN report — or a bulk
    transfer still in flight — leaves the HCD's IPC ticket outstanding until
    the controller event arrives, so the class driver parks instead of
    retrying.
  - `UrbCall` / `UrbClient` — the class-side client: a class driver implements
    `UrbCall` over the kernel `ipc_call` surface (a host test routes the bytes
    straight to `serve_urb`), and
    `UrbClient::{control_in, control_no_data, interrupt_in, bulk_in,
    bulk_out}` build the URB,
    submit it, and decode the completion. A class driver speaks only
    this ABI, so the same binary works behind any controller that serves it —
    it touches no controller register and no other interface's buffer (§5.4,
    `plans/USB.md` §1.3).
  - Bulk endpoints are served through per-direction transfer rings with
    per-slot staging buffers (several TDs may be outstanding per direction,
    completing in order), short packets report the honest byte count, and a
    device STALL is recovered in place — Reset Endpoint → Set TR Dequeue
    Pointer → `CLEAR_FEATURE(ENDPOINT_HALT)` on the device's own EP0 — with
    every abandoned TD answered and the stall surfaced as the distinct
    `EndpointStalled`, so a BOT class driver can run its own recovery.

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
