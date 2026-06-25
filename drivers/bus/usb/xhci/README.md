# `rustos-drv-bus-usb` — xHCI USB host-controller driver (HCD)

`plans/USB.md` U3b. The loadable, autoloaded **host-controller driver**: the
sole owner of one xHCI controller. It maps the controller's register BAR, owns
its DMA rings and root-hub ports, brings it up, enumerates the attached device,
publishes one hardware-tree node per USB interface, and **serves that
interface's transfers** over the bus-agnostic URB transport to an autoloaded
**class** driver (`drivers/input/usb_kbd`, …). It names no class driver, no
board, and no bus (`AGENTS.md` §2.20 / §17.4).

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
  `enumerate_boot_keyboard`, returning the enumerated `UsbDevice`.
- `serve` — `UrbService`, the per-interface state holding at most one
  outstanding interrupt-IN URB (a second concurrent submit fails closed
  `AlreadyExists`), driven on submit/IRQ through `rustos_usb::drive_urb` /
  `frame_completion`; and `attach_transport_grants`, which adds the URB endpoint
  + shared-buffer grants onto the `describe_device` interface node.
- `main.rs` — the freestanding `Run` program: `from_grants_query` → bring-up →
  `shm_create` (the URB data buffer) + grant-restricted `call_create` (the URB
  transport endpoint) → emit the interface node carrying both grants
  (`hw_emit_node` returns the assigned node id) → enable the completion
  interrupter + `irq_bind` → an **asynchronous wait-set event loop** that parks
  on the URB endpoint **and** the controller IRQ: a URB submit is driven and
  either replied at once or held outstanding; a controller interrupt drains the
  event ring, replies the now-complete URB (bounce-copying the report into the
  shared buffer), and watches the root-hub port to retract the interface node
  on disconnect. It never busy-polls (`AGENTS.md` §2.23).

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

- One enumerated device per controller engine (`UsbDevice` drives a single HID
  device's slot + interrupt-IN endpoint). Re-enumeration on re-attach and
  hub-downstream disconnect detection are staged (`plans/USB.md` U5).

## Test surface

The xHCI protocol layers (bring-up, port/doorbell decode, ring state machines,
DMA programming, the full HID enumeration chain, the `ReportSource` report path,
and the URB transport) are tested in `lib/usb` (`cargo test -p rustos-usb`).

`cargo test -p rustos-drv-bus-usb` exercises the pieces here: the `bringup`
fail-closed paths up to the controller hand-off (the inert mock window faults —
the on-metal boundary), and the `serve` `UrbService` state machine (held
interrupt-IN completed on a later event; synchronous control-IN; second-submit
`AlreadyExists`; illegal/fail-closed URBs; idle event) plus the interface-node
grant builder, over a mock engine.
