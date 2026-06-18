# `rustos-virtio-input`

Arch-neutral, transport-agnostic virtio-input (keyboard / pointer) device
logic: the virtio-1.1 §5.8 open/poll/decode engine over the bus-agnostic
`lib/virtio` `Transport`. It lives in `lib/*` so both the in-kernel `-M virt`
input verticals and the user-space input-driver process compose it without a
`drivers/*`→`drivers/*` dependency (`AGENTS.md` §17.4 / §2.2 — the virtio
analogue of `lib/hid` ↔ `drivers/input/usb_hid`). The thin
`drivers/input/virtio_input` crate keeps only the §8 `register` entry and the
§18.3 bind table built from `VIRTIO_INPUT_DEVICE_ID`.

See `docs/src/drivers/input.md` for the full description and test surface.

## Public surface

- `VirtioInput` — the device over a `lib/virtio` `Transport`: `open` (the
  virtio-1.1 §3.1 init sequence + event-buffer pool), `poll`
  (`rustos_abi::driver::input::Input`, interrupt-driven drain, never a busy
  spin), `close`, and `transport_mut` (in-process software-peer drive).
- `VIRTIO_INPUT_DEVICE_ID` — the virtio device id (18) the driver crate's
  `BIND_KEYS` match key is built from (the single source of truth, §2.2).

## Dependencies

`lib/abi`, `lib/virtio` — all `lib/*` (§17.4). Names no board, PCI, or SoC
detail (`AGENTS.md` §2.20); the bus-agnostic `Transport` abstracts the
transport, so the same source binds a virtio-input device however it is
attached.

## Stability

Tier: `experimental`. The open/poll/decode surface is still evolving alongside
the `plans/PI.md` 5d-2-ii user-space input-driver bring-up; `abi-v1` types it
exchanges are governed by `lib/abi`.

## Tests

`cargo test -p rustos-virtio-input` — decode, poll-drain, and teardown unit
tests against the in-process `lib/virtio` `MockTransport` / `MockHost`
(`AGENTS.md` §7).
