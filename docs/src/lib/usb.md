# `tairix-usb`

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
  Enable Slot → Address Device → an 8-byte `GET_DESCRIPTOR` prefix whose
  validated `bMaxPacketSize0` drives an Evaluate Context EP0 fix-up when
  it differs from the speed's assumed worst case (a full-speed receiver's
  8-byte EP0) → the full `GET_DESCRIPTOR` reads (the configuration at its
  exact advertised `wTotalLength`) → `SET_CONFIGURATION` →
  `SET_PROTOCOL(boot)` + `SET_IDLE(indefinite)` per HID interface (the
  latter so an idle report endpoint reports only on change instead of
  storming the controller with duplicate reports) → Configure Endpoint, into a
  growable table of concurrently served **interfaces**, each with its own
  demand-allocated DMA region (EP0 / interrupt / bulk rings and buffers)
  claimed on attach and released on detach — the only concurrency bounds
  are the controller's reported slot count and genuine memory exhaustion,
  never a compile-time budget. A composite
  device — a wireless keyboard+mouse receiver — occupies one entry per
  served interface, the siblings sharing its slot and EP0
  (`InterfaceInfo::decode_all` decodes every default-alternate interface).
  `next_report(index, …)` arms one
  interrupt-IN transfer for the class URB device `index` is currently serving,
  and `engine_for(index)` is the per-device `UrbEngine` view the HCD's URB
  service drives — one interface's transfers can never reach another
  device's endpoints. Endpoint DCIs, packet sizes, and intervals are read
  from each device's descriptors (never hard-coded). A *successful*
  zero-length completion (a ZLP — an idle or composite HID interface, e.g. a
  wireless MMO mouse's extra collection, completing an armed transfer with no
  data) is not a report and not a fault: `next_report` re-arms the endpoint
  and returns `Ok(None)`, so the URB stays parked and a ZLP costs one
  controller interrupt rather than a reply-and-resubmit spin; a genuine
  per-report fault still surfaces after the ring is retired.
  `bring_up` is the arch-neutral bring-up orchestration the host-controller
  driver runs once: it powers all root ports, parks through the connect
  debounce, and attaches **every** connected root port (`attach_root_port`).
  A root device that is itself a hub (the Pi 4B's onboard USB2 hub) is
  installed, descended — every connected downstream port, nested tiers
  included — and watched; a directly-attached device (the Pi 4B's USB3 side
  of each jack is wired straight to a root port) is served beside it (settle
  windows supplied by the `tairix_abi::Delay` seam) — a keyboard and a
  storage stick plugged in together are both served, neither displacing the
  other, and a port whose device fails enumeration is skipped with its slot
  released, never allowed to cost the other devices their service. A device
  absent at bring-up is a first-class state, not a failure: the controller
  comes up watched (each hub's status-change endpoint, and the root ports'
  latched connect changes serviced by `next_root_change` on every interrupt
  wake, with no controller reset), so a cold boot with nothing plugged in
  works and each device autoloads when plugged in.
- `regs` / `trb` / `ring` — the register, TRB, and ring-state vocabularies; the
  ring state machines (`ProducerRing`, `EventRingCursor`) hold no memory of
  their own, so the owner publishes every write through the `device::DmaBank`
  seam.
- `SlabBank` — the production `device::DmaBank`: a growable bank of owned DMA
  chunks minted from the host's `DmaHost` seam. The engine's first chunk holds
  the controller-shared structures, sized exactly to the reported geometry
  (`MaxSlots`, context size, the VL805's 31-page scratchpad); every served
  device's rings/buffers live in a chunk grown on attach and released on
  detach, and each allocation is verified against the controller's inbound
  DMA aperture, failing closed on a chunk the silicon could not reach (§2.2,
  §24.1).
- `XHCI_COMPATIBLE` — the `compatible` identity (`usb,xhci`) a discovered xHCI
  controller node carries (§18.1). An xHCI-protocol identity (not a board or
  vendor name), so it lives here as the single definition the emitting bus
  driver (`drivers/bus/usb/vl805`, which publishes the controller node under
  it) and the binding controller driver (`drivers/input/usb_kbd`'s
  `KEYBOARD_BIND_KEYS`) share (§2.2 / §2.20).

- `transport` — the **bus-agnostic URB transport seam** the modular USB stack
  (`plans/USB.md`) is built on. The wire contract is `tairix_abi::usb_urb`: a
  `UrbRequest` (endpoint, transfer type, direction, shared-buffer handle,
  length, control SETUP) and a status-framed completion (bytes transferred, or
  an in-band `Errno`). `transport` adds the two ends both sides share:
  - `UrbEngine` — the controller-side operation seam the HCD's live engine
    performs (`UsbDevice` implements it: `control_in` over the EP0 control
    transfer — targeting the enumerated *device*, switching a hub-downstream
    device's EP0 ring active for the transfer — `control_no_data` over the
    same path for a SETUP-only class request (the BOT Mass Storage Reset,
    `plans/DEVICES.md` D2), `control_out` for a class request carrying an
    OUT data stage (the CBI ADSC command channel, `plans/DEVICES.md` D5),
    `interrupt_in` over the `ReportSource` report poll — a HID report
    endpoint or a CBI interface's completion endpoint alike — and
    `bulk_in` / `bulk_out` over the interface's configured bulk endpoints:
    the IN/OUT pair a BOT/CBI interface carries, or the two pairs a UAS
    interface's four pipes need (`plans/DEVICES.md` D1/D5), addressed by
    endpoint number and routed to the matching per-pipe ring).
  - `drive_urb` — the controller-side server transformation: decode a URB,
    validate it fail-closed against the interface (control ⇒ endpoint 0,
    served as IN, the zero-length no-data OUT, or the data-stage OUT
    carrying the shared buffer's bytes; interrupt/bulk ⇒ a device
    endpoint; an oversize length or a
    malformed frame is refused **before** the engine is touched), drive the
    engine over the shared buffer, and frame the
    completion in band. A not-yet-arrived interrupt-IN report — or a bulk
    transfer still in flight — leaves the HCD's IPC ticket outstanding until
    the controller event arrives, so the class driver parks instead of
    retrying.
  - `UrbCall` / `UrbClient` — the class-side client: a class driver implements
    `UrbCall` over the kernel `ipc_call` surface (a host test routes the bytes
    straight to `serve_urb`), and
    `UrbClient::{control_in, control_no_data, control_out, interrupt_in,
    bulk_in, bulk_out}` build the URB,
    submit it, and decode the completion. A class driver speaks only
    this ABI, so the same binary works behind any controller that serves it —
    it touches no controller register and no other interface's buffer (§5.4,
    `plans/USB.md` §1.3).
  - Bulk endpoints are served through per-pipe transfer rings with
    per-slot staging buffers (several TDs may be outstanding per pipe,
    completing in order; a UAS interface's second pair shares the
    direction's staging buffers — the URB service holds one URB in flight
    per interface, so the pipes never race on them), short packets report
    the honest byte count, and a device STALL is recovered in place —
    Reset Endpoint → Set TR Dequeue Pointer →
    `CLEAR_FEATURE(ENDPOINT_HALT)` on the device's own EP0 — with
    every abandoned TD answered and the stall surfaced as the distinct
    `EndpointStalled`, so a storage class driver can run its own recovery.
    A STALLed *control* transfer is likewise recovered in place (Reset
    Endpoint + a rebuilt EP0 ring; the device side self-clears at the next
    SETUP) and surfaced as `EndpointStalled` — the CBI "command not
    accepted" answer — with the observed completion code preserved for the
    diagnostics.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_op_in_unsafe_fn)]`, `lib/abi`-only.
- Every controller and DMA access is mediated by the `XhciHost` /
  `device::DmaBank` seams, so the bring-up, enumeration, and ring state
  machines are proven host-side against a register-level mock plus an in-memory
  ring/DMA model (§2.2); the doorbell below them is the on-metal acceptance
  item (no QEMU `raspi*` USB vertical exists, §0.4).
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
- The crate holds **no** capability of its own — authority is the consuming
  driver's (`CAP_MMIO_MAP` for the register window, `CAP_MEM_DMA` for the DMA
  carve), checked in the wiring that mints them.

## Stability

Tier: `experimental`.
