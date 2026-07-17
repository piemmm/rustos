# `tairix-drv-input-virtio-input` — virtio-input keyboard / pointer driver shell

The thin §8 driver shell for a virtio-input device (virtio 1.1 §5.8) — the
paravirtualised input device QEMU exposes as `virtio-keyboard-device` /
`virtio-mouse-device` / `virtio-tablet-device` on every machine type, and the
input class real virtio hardware presents. Per `AGENTS.md` §8 the only public
*function* is `register`, and `BIND_KEYS` is the §18.3 bind table.

The arch-neutral, transport-agnostic open/poll/decode device logic (the
`VirtioInput` engine and the `evdev`→`InputEvent` decode) lives in
`lib/virtio_input` (`tairix_virtio_input`), so both this driver and the
user-space input-driver process compose it without a `drivers/*`→`drivers/*`
dependency (`AGENTS.md` §17.4 / §2.2 — the virtio analogue of `lib/hid` ↔
`drivers/input/usb_kbd`). See that crate's README and
`docs/src/drivers/input.md` for the wire protocol, event encoding, and polling
model.

## Supported hardware

| Platform | Transport          | Device                         | Status                        |
|----------|--------------------|--------------------------------|-------------------------------|
| any      | virtio-MMIO / PCI  | virtio-input (device id 18)    | mock-host + aarch64 & riscv64 QEMU verticals |

The match key carries no transport detail, so the same driver binds whether the
device is attached over virtio-MMIO or PCI (the bus-agnostic `lib/virtio`
`Transport` abstracts the bus, `AGENTS.md` §2.2).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MEM_DMA` is required of the loaded image so the driver host's virtio
  factory mints a DMA-capable `VirtioHost` (the event buffers and virtqueue
  rings, allocated by `lib/virtio_input`, are device-visible memory).
- `Input::poll` is gated by ownership of the `DriverHandle` returned from
  `register`; the `Input` trait declares no additional per-method capability.

The driver runs in user space and does **not** request `CAP_DRV_KERNEL`. It
holds no ambient authority over MMIO or the DMA pool: every device-visible
buffer is allocated through the host-supplied `VirtioHost` DMA seam and the
device is reached only through the `Transport` seam (`AGENTS.md` §4 / §17.4).

## Discovery binding

The driver publishes a canonical `BIND_KEYS` table (`AGENTS.md` §18.3): one
entry matching a probed virtio node whose device id is `virtio-input`
(`HwMatchKey::virtio(18)`), at the exact-match priority tier. The device id
comes from `tairix_virtio_input::VIRTIO_INPUT_DEVICE_ID` — the single source of
truth the device logic, this bind table, and the driver's signed-manifest bind
table are all authored from (`AGENTS.md` §2.2) — and is the data `devmgr` (or
the in-kernel bootstrap-floor catalogue) resolves a discovered virtio-input
node against.

## Test surface

`cargo test -p tairix-drv-input-virtio-input` exercises:

- the `register` capability gate, and
- the §18.3 `BIND_KEYS` table matching a probed `virtio-input` node (device id
  18) and rejecting a different virtio device id or a non-virtio (PCI) key.

The open/poll/decode device-logic tests (decode, poll-drain, teardown) live with
the logic in `lib/virtio_input` (`AGENTS.md` §2.2 / §17.4).

Two QEMU integration verticals — `input_virtio_mmio_qemu_aarch64`
(`tairix-test-input-virtio-mmio-qemu-aarch64`) and
`input_virtio_mmio_qemu_riscv64` (`tairix-test-input-virtio-mmio-qemu-riscv64`),
both enrolled in `cargo xtask test --qemu` — boot the production kernel on the
aarch64 / riscv64 `virt` board, build the virtio-MMIO transport from the device
tree, arm the device IRQ path (aarch64 GICv2 SPI + EL1 / riscv64 PLIC source +
S-mode trap), load this driver's signed `.rxe` through `tairix_drvhost::Host`,
and drive `lib/virtio_input`'s `VirtioInput` through load → use → unload →
reload. "Use" is made deterministic by a real injected key: the QEMU runner
attaches a `virtio-keyboard-device`, waits for the guest to log the
event-queue-armed readiness marker on the serial console, then sends a key
through the QEMU monitor (`sendkey`); the guest's eventq IRQ fires and the
driver decodes the resulting press and, after reload, the matching release.
Both verticals run the same driver source and the same shared
`virtio_input_keypress` key-decode tail (`AGENTS.md` §2.2).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. `BIND_KEYS` is the
§18.3 bind-table data described above. The `VirtioInput` device type lives in
`lib/virtio_input`.
