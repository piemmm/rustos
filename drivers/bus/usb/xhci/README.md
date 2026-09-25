# `tairix-drv-bus-usb` — xHCI USB host-controller driver (HCD)

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
[`tairix-usb`](../../../../lib/usb) (`lib/usb`), so this driver and the class
drivers build on the same engine without depending on each other (`drivers/* →
lib/*` only; the USB analogue of `lib/virtio` ↔ `drivers/bus/virtio`).

## What lives here

- `BIND_KEYS` — the §18.3 bind table: one `compatible(usb,xhci)` key, so it
  autoloads against the `usb,xhci` node the VL805 bus driver emits.
- `bringup` — `derive_controller_resources` + `bring_up_controller[_diagnostic]`:
  derive the BAR/DMA bounds from the granted resources, carve+aperture-check
  the DMA region (fail-closed `OutOfRange`, §5.4), map the BAR, and bring the
  controller up via `tairix_usb::Xhci::open` + `UsbDevice::start` (which also
  enables the completion interrupter — the engine's synchronous waits park on
  the caller-supplied `EventWait` seam, wall-clock-bounded, never an
  iteration-count spin) + `UsbDevice::bring_up`, returning the `UsbDevice`
  engine serving every enumerated device — or serving with its first-connect
  watch armed when none is yet attached.
- `serve` — `UrbService`, the per-interface state holding at most one
  outstanding interrupt-IN URB (a second concurrent submit fails closed
  `AlreadyExists`), driven on submit/IRQ through `tairix_usb::drive_urb` /
  `frame_completion`; a retracted node's parked URB is answered `NotFound`, as
  is any submit on an endpoint carrying no node. While the controller is
  recovering nothing is driven: a report poll is held for the next reset and
  any other transfer is answered with the reissuable `WouldBlock`
  (`reissue_held_transfer` does the same for a transfer already held when a
  reset fails). `attach_transport_grants` adds the URB endpoint + shared-buffer
  grants onto the `describe_device` interface node.
- `interfaces` — `Interfaces`, the published nodes and the transports they
  ride, driven over the `Seam` trait (the live engine and syscalls in the `Run`
  binary, a mock in the tests). `reconcile` keeps a node while the device it
  was built from (`tairix_usb::device::DeviceIdentity`: bus position,
  descriptor identity, serial number) is served, following it to whatever
  index a controller reset gave it; retracts the rest; then publishes the
  unclaimed devices, each on a drained endpoint and a shared buffer created for
  that node alone, since the kernel retires a removed node's regions. A serial
  that no longer reads counts as another device. `serve_submit`, `drive_busy`,
  and `recover` hold the URB and recovery sequencing, including recovery after
  a transfer fault proves an unplug on either the submit or the interrupt path.
- `domain` — `ControllerHealth`, the controller's interior fault domain (the
  recovery grace window) and the one deferred re-attach owed a port the
  bring-up walk skipped.
- `main.rs` — the freestanding `Run` program: `from_grants_query` → `irq_bind`
  (the controller's interrupt line, **before** the controller is touched — the
  engine's synchronous waits park on it through the `EventWait` seam, and the
  interrupter is enabled as part of `UsbDevice::start`) → bring-up (a failure
  logs the **whole** diagnostic breadcrumb: phase, error, open stage,
  `USBCMD`/`USBSTS`, enumeration stage, completion/event-type/reject codes,
  and `PORTSC`; success logs a topology summary and warns about connected
  devices that failed enumeration and were skipped) → per served interface, a
  grant-restricted `call_create` endpoint (reused across nodes) and a fresh
  `shm_create` buffer → emit the interface node carrying both grants
  (`hw_emit_node` returns the assigned node id) → an **asynchronous wait-set
  event loop** that parks —
  unbounded, with no periodic wakes — on the URB endpoint **and** the
  controller IRQ: a URB submit is driven and either replied at once or held
  outstanding; a controller interrupt drains the event ring, services
  hot-plug before stale transfer completions, and replies the now-complete
  URB (bounce-copying the report into the shared buffer). The hot-plug path
  is the onboard hub's status-change watch (`next_hub_change`: enumerate a
  freshly-connected device and publish a node, or abort the parked URB,
  retract on disconnect, and reject stale old-driver submits while absent)
  — plus a fault-confirmation path for controllers that report unplug first as the
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
  the watch before the later reconnect. Root-port connects/disconnects — a
  directly-attached device (the Pi 4's USB3 side of each jack), or a whole
  hub assembly pulled from its port — are serviced from the `PORTSC.CSC`
  latches on every interrupt wake (`UsbDevice::next_root_change`): a new
  connect is attached in place and a disconnect detaches exactly what the
  port carried, with no controller reset, so sibling ports' devices are
  untouched. Every (re)attach publishes a fresh node with a buffer no other
  node carried, so `devmgr` autoloads a class driver that sees nothing of the
  previous device. It never busy-polls (`AGENTS.md` §2.23).
- **Controller recovery keeps the devices that come back.** A controller that
  latches `USBSTS.HSE`/`HCHalted` is reset and re-enumerated with its interface
  nodes left published, and after the walk each node is matched to its device
  by identity wherever the walk placed it (`Interfaces::reconcile`), so an
  unchanged device keeps its node, its id, its buffer, and its bound class
  driver even when an earlier unplug's hole moved it to another index. Node
  ids are never reissued, so this is the only way a device survives the reset
  with its driver. Held URBs are answered once their device's fate is known:
  `WouldBlock` where it came back, `NotFound` where its node was retracted. A
  reset that fails leaves the nodes published and serves nothing through the
  controller while its grace window runs (held transfers answered
  `WouldBlock`, report polls kept); the window's one-shot retries the reset.
  If the window elapses first, every interface node is retracted, so `devmgr`
  unloads their drivers, and the HCD exits (code 85) with the reason logged,
  handing the controller's memory to the kernel to quarantine. A node is
  published only after any URB still queued on its endpoint has been answered
  `NotFound`: with no node on it, only a previous node's driver can have posted
  it.

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
and the URB transport) are tested in `lib/usb` (`cargo test -p tairix-usb`).

`cargo test -p tairix-drv-bus-usb` exercises the pieces here: the `bringup`
fail-closed paths up to the controller hand-off (the inert mock window faults —
the on-metal boundary), the `serve` `UrbService` state machine (held
interrupt-IN completed on a later event; synchronous control-IN; second-submit
`AlreadyExists`; aborting a parked URB on disconnect before stale transfer
faults are drained; rejecting a stale submit after interface removal;
illegal/fail-closed URBs; idle event; a report poll held and every other
transfer answered `WouldBlock` without touching the controller while it
recovers, and a failed reset answering a held transfer `WouldBlock` while a
held report poll keeps waiting) plus the interface-node grant builder, over a
mock engine; the `interfaces` table over a mock seam that journals every
kernel step and retires a removed node's region (a device a reset moved to
another index, or reordered, keeps its node and buffer; a moving node never
takes an index another node serves; a re-plug at the same index is published
on a fresh region; publication only after the endpoint is drained and any
retraction; every node decided when memory runs out; a refused node releases
its region; an unwatched endpoint watched again, never re-bound; any identity
fact changing replaces the node; serial numbers telling twins apart; a fault
detach on the submit path recovering the controller; a controller that misses
its grace window retracting everything and never being reset again); and the
`domain` grace-window machine.
`cargo test -p tairix-usb` also covers confirming a watched
hub-downstream detach from a failed report transfer while preserving ordinary
live-device report faults, and re-arming a stashed hub status-change completion
or delayed disconnect latch so a later reconnect re-enumerates.
