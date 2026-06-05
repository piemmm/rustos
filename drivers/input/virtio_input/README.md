# `rustos-drv-input-virtio-input` — virtio-input keyboard / pointer driver

Stage W11 deliverable. Implements `rustos_abi::driver::input::Input` for
a virtio-input device (virtio 1.1 §5.8) — the paravirtualised input
device QEMU exposes as `virtio-keyboard-device` /
`virtio-mouse-device` / `virtio-tablet-device` on every machine type,
and the input class real virtio hardware presents. The driver is
bus-agnostic: the same source compiles against the PCI and MMIO
transports in `drivers/bus/virtio` (the queue protocol lives once, in
`lib/virtio` — `AGENTS.md` §2.2).

## Supported hardware

| Platform | Transport          | Device                         | Status                        |
|----------|--------------------|--------------------------------|-------------------------------|
| any      | virtio-MMIO / PCI  | virtio-input (device id 18)    | mock-host + aarch64 & riscv64 QEMU verticals |

This driver consumes the device-to-driver **event queue** (queue 0). It
does not program the driver-to-device **status queue** (queue 1): that
queue carries host feedback (LEDs, force-feedback), which the `abi-v1`
`Input` surface does not model. No feature bits are negotiated.

### Event encoding

The wire record is `struct virtio_input_event { __le16 type; __le16
code; __le32 value; }`. `type`/`code` are the Linux `evdev` namespaces,
which `poll` maps onto the platform-neutral `InputEvent`:

| evdev `type` | evdev `code`        | `InputEvent.kind` | `code`        | `value`        |
|--------------|---------------------|-------------------|---------------|----------------|
| `EV_KEY` (1) | keycode             | `Key`             | keycode       | 1 press / 0 release / 2 repeat |
| `EV_REL` (2) | `REL_X` (0)         | `Pointer`         | `0` (X axis)  | signed delta   |
| `EV_REL` (2) | `REL_Y` (1)         | `Pointer`         | `1` (Y axis)  | signed delta   |
| `EV_REL` (2) | `REL_WHEEL` (8)     | `Scroll`          | `1` (Y axis)  | signed delta   |

`EV_SYN` frame separators and any `type`/`code` outside the table are
consumed but surface no event (`poll` returns `Ok(0)` for that drain),
so the driver never fabricates a bogus event (`AGENTS.md` §2.9).

The keycode in `EV_KEY` events is the `evdev` keycode (e.g. `KEY_A =
30`), which is the platform-neutral keycode the `Input` trait already
documents — unlike the `ps2` driver's scancode-set-1 codes. Keymap
translation, repeat synthesis, and modifier/lock state are
higher-layer concerns.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MEM_DMA` is required of the loaded image so the driver host's
  virtio factory mints a DMA-capable `VirtioHost` (the event buffers
  and virtqueue rings are device-visible memory).
- `Input::poll` is gated by ownership of the `DriverHandle` returned
  from `register`; the `Input` trait declares no additional per-method
  capability.

The driver allocates every device-visible buffer through the
host-supplied `VirtioHost` DMA seam and reaches the device only through
the `Transport` seam; it holds no ambient authority over MMIO or the
DMA pool (`AGENTS.md` §4 / §17.4). It runs in user space and does
**not** request `CAP_DRV_KERNEL`.

## Lifecycle

`register` clears the load-time gate; `VirtioInput::open` runs the
virtio 1.1 §3.1 init sequence (reset → ACKNOWLEDGE → DRIVER →
features → `FEATURES_OK` → event-queue setup → `DRIVER_OK`);
`VirtioInput::close` resets the device (the unload step). Reloading is
constructing a fresh instance over the same transport, which the
aarch64 and riscv64 QEMU verticals exercise through
load → use → unload → reload.

### Polling model

`poll` posts one device-writable 8-byte event buffer, waits through the
host's `notify_wait` (interrupt-driven on real hardware — the caller's
IRQ waiter parks the CPU, so there is no busy-spin, `AGENTS.md` §2.1),
then decodes the one completed event. It returns `Ok(1)` for a mapped
event, `Ok(0)` for a frame marker / unmodelled event / spurious wake,
and drains a multi-event burst across successive calls. QEMU buffers
events internally until a descriptor is available, so no event is lost
between calls.

## Test surface

`cargo test -p rustos-drv-input-virtio-input` exercises, against the
in-process `MockTransport`/`MockHost` doubles:

- `register` capability gate.
- Empty-buffer rejection (`BufferTooSmall`) and empty-queue `Ok(0)`.
- Key press / release decode, in order.
- Relative pointer (X / Y) and scroll-wheel decode.
- Frame-marker (`EV_SYN`) and unmodelled-event discard.
- `open` → `close` (load → unload) round-trip.

Two QEMU integration verticals — `input_virtio_mmio_qemu_aarch64`
(`rustos-test-input-virtio-mmio-qemu-aarch64`) and
`input_virtio_mmio_qemu_riscv64`
(`rustos-test-input-virtio-mmio-qemu-riscv64`), both enrolled in `cargo
xtask test --qemu` — boot the production kernel on the aarch64 /
riscv64 `virt` board, build the virtio-MMIO transport from the device
tree, arm the device IRQ path (aarch64 GICv2 SPI + EL1 / riscv64 PLIC
source + S-mode trap), load this driver's signed `.rxe` through
`rustos_drvhost::Host`, and drive it through load → use → unload →
reload. "Use" is made deterministic by a real injected key: the QEMU
runner attaches a `virtio-keyboard-device`, waits for the guest to log
the event-queue-armed readiness marker on the serial console, then
sends a key through the QEMU monitor (`sendkey`); the guest's eventq
IRQ fires and the driver decodes the resulting press and, after reload,
the matching release. Both verticals run the same driver source and the
same shared `virtio_input_keypress` key-decode tail (`AGENTS.md` §2.2).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`VirtioInput` type and its `open` / `close` / `transport_mut` members
are re-exported so the driver host (and the QEMU vertical) can
construct and drive an instance; the host reaches it only through the
`Input` trait afterwards.
