# `rustos-drv-bus-usb` — xHCI USB host-controller driver (HCD)

`plans/USB.md` U3b. The loadable, autoloaded **host-controller driver**: the
sole owner of one xHCI controller. It maps the controller's register BAR, owns
its DMA rings and root-hub ports, brings it up, enumerates the attached device,
publishes one hardware-tree node per USB interface once a device is present,
and **serves that interface's transfers** over the bus-agnostic URB transport to
an autoloaded **class** driver (`drivers/input/usb_kbd`, …). It names no class
driver, no board, and no bus (`AGENTS.md` §2.20 / §17.4). A device absent at
boot is a first-class state: the controller comes up and waits for the first
hot-plug connect (the onboard hub's status-change watch, or a root-port
connect), so a cold boot with the keyboard unplugged works.

The crate is a `lib` (host-testable logic) **and** a `Run` binary (the process).

The bus-agnostic xHCI **protocol** (the `XhciHost` register seam, the `Xhci`
controller engine, the TRB/ring vocabulary, the `UsbDevice` enumeration engine,
and the URB transport `drive_urb`/`UrbEngine`) lives in
[`rustos-usb`](../../../../lib/usb) (`lib/usb`), so this driver and the class
drivers build on the same engine without depending on each other (`drivers/* →
lib/*` only; the USB analogue of `lib/virtio` ↔ `drivers/bus/virtio`).

## What lives here

- `BIND_KEYS` — the §18.3 bind table: one `compatible(usb,xhci)` key, so it
  autoloads against the `usb,xhci` node the VL805 bus driver emits.
- `bringup` — `derive_controller_resources` + `bring_up_controller[_diagnostic]`:
  derive the BAR/DMA bounds from the granted resources, carve+aperture-check
  the DMA region (fail-closed `OutOfRange`, §5.4), map the BAR, and bring the
  controller up via `rustos_usb::Xhci::open` + `UsbDevice::start` +
  `UsbDevice::bring_up_keyboard`, returning the `UsbDevice` engine — pointed at
  the device's slot when one is present, or serving with its first-connect watch
  armed (`BringUp::AwaitingDevice`) when none is yet attached.
- `serve` — `UrbService`, the per-interface state holding at most one
  outstanding interrupt-IN URB (a second concurrent submit fails closed
  `AlreadyExists`), driven on submit/IRQ through `rustos_usb::drive_urb` /
  `frame_completion`; a disconnect aborts any parked URB with `NotFound` before
  the interface node is retracted, and any later submit while no interface node
  is live is rejected with `NotFound`, so a replugged class driver starts from a
  clean transport state. `attach_transport_grants` adds the URB endpoint +
  shared-buffer grants onto the `describe_device` interface node.
- `main.rs` — the freestanding `Run` program: `from_grants_query` → bring-up →
  `shm_create` (the URB data buffer) + grant-restricted `call_create` (the URB
  transport endpoint) → emit the interface node carrying both grants
  (`hw_emit_node` returns the assigned node id) → enable the completion
  interrupter + `irq_bind` → an **asynchronous wait-set event loop** that parks
  on the URB endpoint **and** the controller IRQ: a URB submit is driven and
  either replied at once or held outstanding; a controller interrupt drains the
  event ring, services hot-plug before stale transfer completions, and replies
  the now-complete URB (bounce-copying the report into the shared buffer). The
  hot-plug path is the onboard hub's status-change watch (`next_hub_change`:
  enumerate a freshly-connected device and publish a node, or abort the parked
  URB, retract on disconnect, and reject stale old-driver submits while absent)
  plus a fault-confirmation path for controllers that report unplug first as the
  watched device's failed interrupt transfer; that path retracts when the
  device's own endpoint reported a device-unreachable completion code (a USB or
  split transaction error — conclusive on its own, since the gone device's hub
  often cannot answer a port-status read), else falls back to reading the hub
  port and retracting only when it is now disconnected. The slot teardown is
  **best-effort**: it issues a Disable Slot but frees the local slot state even
  if the gone device's hub never lets the controller confirm it (otherwise the
  device would stay tracked and a re-plug would be ignored), so a re-plug always
  re-enumerates. It leaves the hub's
  connection-change latch for the status endpoint to report, so a delayed
  disconnect notification still wakes the loop, drains the latch, and re-arms
  the watch before the later reconnect. A directly-attached device uses the
  root-port connect/disconnect (`any_root_port_connected` →
  `reset_and_reenumerate`). Every (re)attach publishes a fresh node carrying the
  same transport grants so the class driver re-autoloads onto the same sink. It
  never busy-polls (`AGENTS.md` §2.23).

## Least privilege (`AGENTS.md` §5.4)

`CAP_MMIO_MAP` (register BAR), `CAP_MEM_DMA` (controller DMA ring), `CAP_IRQ_BIND`
(completion interrupt), `CAP_SHM` (the URB data buffer), `CAP_IPC_BIND_PRIVILEGED`
(the restricted-sender URB endpoint), `CAP_HW_EMIT` (publish the interface
node), `CAP_LOG_EMIT` (one-shot diagnostic). It runs in user space and does not
request `CAP_DRV_KERNEL`. The class driver it serves holds **none** of these —
only the right to submit URBs on its one interface and map its one buffer.

## Supported hardware

| Platform | Controller                    | Status |
|----------|-------------------------------|--------|
| Pi 4     | VL805 PCIe xHCI (USB-A ports) | protocol + bring-up + URB-serve logic host-proven; live enumerate/serve is the metal acceptance item (`plans/USB.md` U5) |

The register window and DMA constraint arrive as grants on the matched
`usb,xhci` node — never a compiled-in base (`AGENTS.md` §18.1). QEMU models no
Pi USB timing, so the emulation artefact is the host test suite and the live
controller behaviour is a metal checklist (`plans/PI.md` §0.4).

## Limitations

- Concurrently served devices are bounded by the controller's reported
  slot count (`HCSPARAMS1.MaxSlots`, the same bound the silicon imposes on
  any host) and genuine memory exhaustion: each device's DMA region and
  its URB transport are allocated when it attaches and released when it
  detaches, never a fixed table. Event-driven hot-plug — hub-downstream
  connect/disconnect on any tier, directly-attached connect/disconnect,
  fresh re-enumeration, and cold boot with no device attached — is built
  and host-proven (`plans/USB.md` U5/U9); live attach/detach/cold-boot
  acceptance is metal-only (QEMU models no Pi USB).
- Hubs are descended recursively (a hub plugged into a hub, up to the xHCI
  route string's five tiers): each tier is installed, marked, and watched on
  its own status-change endpoint, an unplugged hub cascades the teardown of
  everything behind it, and a hot-plugged hub is descended in place
  (`plans/USB.md` U9).

## Test surface

The xHCI protocol layers (bring-up, port/doorbell decode, ring state machines,
DMA programming, the full HID enumeration chain, the `ReportSource` report path,
and the URB transport) are tested in `lib/usb` (`cargo test -p rustos-usb`).

`cargo test -p rustos-drv-bus-usb` exercises the pieces here: the `bringup`
fail-closed paths up to the controller hand-off (the inert mock window faults —
the on-metal boundary), and the `serve` `UrbService` state machine (held
interrupt-IN completed on a later event; synchronous control-IN; second-submit
`AlreadyExists`; aborting a parked URB on disconnect before stale transfer
faults are drained; rejecting a stale submit after interface removal;
illegal/fail-closed URBs; idle event) plus the interface-node grant builder,
over a mock engine. `cargo test -p rustos-usb` also covers confirming a watched
hub-downstream detach from a failed report transfer while preserving ordinary
live-device report faults, and re-arming a stashed hub status-change completion
or delayed disconnect latch so a later reconnect re-enumerates.
