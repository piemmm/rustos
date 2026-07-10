# `rustos-virtio-input`

`lib/virtio_input` is the arch-neutral, transport-agnostic virtio-input
(keyboard / pointer) device logic the virtio-input driver is built from: the
virtio-1.1 §5.8 open/poll/decode engine over the bus-agnostic `lib/virtio`
`Transport`. It lives in `lib/*` — not in a driver crate — so **both** the
in-kernel `-M virt` input verticals and the user-space input-driver process
(`rustos-drv-input-virtio-kbd`) compose it without a `drivers/*`→`drivers/*`
dependency (`AGENTS.md` §17.4 / §2.2), exactly as the bus-agnostic xHCI protocol
lives in
[`rustos-usb`](./usb.md) rather than the xHCI driver, and the HID logic lives
in [`rustos-hid`](./hid.md) rather than the USB HID class drivers. The thin
`drivers/input/virtio_input` crate keeps only the §8 `register` entry and the
§18.3 bind table.

## What it provides

- **`VirtioInput`**: the device over a `lib/virtio` `Transport`. `open` runs the
  virtio-1.1 §3.1 initialisation sequence (negotiating only
  `VIRTIO_F_VERSION_1`, the modern split-virtqueue layout — no device-specific
  features) and pre-posts a pool of device-write event buffers keyed by the
  descriptor head the queue assigns. A single posted buffer is not enough: the
  device fills one buffer per event of a report, so a keypress's `EV_KEY` *and*
  its trailing `EV_SYN` each need a free buffer at once.
- **`poll`** (`rustos_abi::driver::input::Input`): drains every completed event,
  decodes it, and hands each buffer straight back so the pool stays full. The
  wait is interrupt-driven through the host's `notify_wait` — never a busy spin
  (`AGENTS.md` §2.1). An empty caller buffer is `DriverError::BufferTooSmall`;
  the engine never panics (`AGENTS.md` §2.9).
- **`evdev` → `InputEvent` decode**: the wire record is
  `struct virtio_input_event { __le16 type; __le16 code; __le32 value; }`
  (virtio 1.1 §5.8.6) in the Linux `evdev` namespaces, mapped onto the
  platform-neutral `InputEvent`: `EV_KEY` → `Key` (the evdev keycode, `value`
  1 press / 0 release), `EV_REL` `REL_X`/`REL_Y` → `Pointer`, and `REL_WHEEL`
  → `Scroll`. `EV_SYN` frame separators and any unmodelled `type`/`code` are
  consumed but surface no event, so the engine never fabricates a bogus one
  (`AGENTS.md` §2.9 — fail closed, never guess).
- **`VIRTIO_INPUT_DEVICE_ID`**: the virtio device id (18) the driver crate's
  `BIND_KEYS` match key is built from — the single source of truth the device
  logic and the bind table both depend on (`AGENTS.md` §2.2 / §18.3).
- **`VirtioKeyboardConsole`** (`console` module): the keyboard producer half.
  `feed` turns each decoded `evdev`-keycode `Key` edge into the
  `rustos_abi::input::KeyInput` record a driver injects through `key_inject`,
  tracking the held modifiers (each of the eight modifier keys independently,
  collapsing left/right pairs) and the caps-/num-lock toggles and resolving the
  US layout. The `evdev`-keycode→`Key` table is `evdev`-specific, but the
  `Key`→record map is the shared `rustos_keymap::key_input` — the one definition
  the `lib/hid` USB console producer reaches too (`AGENTS.md` §2.2). An unknown
  keycode or non-key event produces no record (fail closed, `AGENTS.md` §2.9).

## Layering and platform-neutrality

`lib/virtio_input` depends only on other `lib/*` crates — `lib/abi` (the
`Input`/`InputEvent`/`BufferClass`/`KeyInput` surface), `lib/virtio` (the
bus-agnostic `Transport`, `SplitQueue`, DMA slabs, and `VirtioHost`), and
`lib/input` + `lib/keymap` (the `Key` vocabulary and the shared `Key`→record
map the console producer uses) — so it satisfies §17.4
and names no board, PCI, or SoC detail (`AGENTS.md` §2.20). It allocates every
device-visible buffer through the `VirtioHost` DMA seam and reaches the device
only through the `Transport` seam, holding no ambient authority (`AGENTS.md`
§4); the same source binds a virtio-input device however it is attached
(MMIO or PCI).

## Test surface

`cargo test -p rustos-virtio-input` exercises, against the in-process
`lib/virtio` `MockTransport` / `MockHost`:

- decode: key press/release, relative pointer (X/Y) and scroll-wheel, and the
  discard of `EV_SYN` frame markers / unmapped codes / unmodelled types;
- poll-drain: a queued press, press-then-release in order, a frame marker
  surfacing no event, the no-pending-event `Ok(0)`, and empty-buffer rejection;
- teardown: the `open` → `close` (load → unload) round-trip;
- `console`: `evdev`-keycode resolution (letters, shifted digits, named and
  keypad keys, function keys), caps/num-lock toggling, left/right modifier
  collapsing, and the fail-closed unknown-keycode / non-key / key-repeat cases.

## Stability

Tier: `experimental` (see the crate `README.md`).
