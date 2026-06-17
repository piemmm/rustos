# `rustos-drv-input-usb-hid` — USB-HID boot-protocol input driver

`plans/PI.md` P10 deliverable (host-provable slice). Decodes the HID
**boot-protocol** keyboard and mouse report formats (USB HID 1.11
Appendix B) into platform-neutral `InputEvent`s
(`rustos_abi::driver::input`). Boot protocol is the fixed report shape
every USB keyboard and mouse must speak without a report-descriptor
parse, so this decoder needs no descriptor parsing.

## Supported hardware

| Device class            | Report format        | Status                          |
|-------------------------|----------------------|---------------------------------|
| USB boot keyboard       | 8-byte input report  | host-proven over a mock source  |
| USB boot mouse          | 3+-byte input report | host-proven over a mock source  |

The decoders are written against the `ReportSource` seam (defined in
`lib/abi` as `rustos_abi::driver::input::ReportSource`, because its
producer is a sibling driver and drivers depend only on `lib/*`,
`AGENTS.md` §17.4), not a concrete USB transfer ring. On metal the
source is the device's interrupt-IN endpoint serviced by the xHCI
driver's `UsbDevice` engine (`drivers/bus/usb`), which enumerates the
device — `SET_PROTOCOL(boot)` included — and implements the seam over
its interrupt-IN transfer ring; the usb crate's end-to-end test polls
a `BootKeyboard` over its mock controller. Report protocol
(descriptor-driven) decoding is out of scope: boot protocol is the
bring-up path.

### Event encoding

- Keyboard keys: `Key` events whose `code` is the HID usage ID (page
  `0x07`); the eight boot modifiers are usages `0xE0..=0xE7`. `value`
  is `1` press / `0` release. The keyboard report carries state, so the
  decoder diffs consecutive reports and emits one event per edge;
  rollover/POST-error reports keep the held-key state and diff only the
  (still valid) modifier byte.
- Mouse buttons: `Key` events with codes `0x110`/`0x111`/`0x112`
  (left/right/middle — the codes a virtio pointer device delivers, so
  the WM sees one button vocabulary).
- Motion: `Pointer` deltas on axes `0` (X) / `1` (Y); wheel motion is
  `Scroll` on axis `1`.

### Console-input producer

For a keyboard wired to a text console, the `console` module turns the
raw usage edges above into the console (tty) bytes a terminal sends.
`KeyboardConsole` tracks the held modifiers and the caps-/num-lock
state, resolves each press to the `rustos_input::Key` a US layout
produces (the HID-usage table is HID-specific; a `ps2` keyboard
resolves scancode set 1 into the same vocabulary), and runs it through
the shared `lib/keymap` terminal map. `pump_once` is the driver loop:
poll the keyboard, feed each event, and inject the bytes through a
`ConsoleSink` — on metal a call to the `console_input` syscall against
the video console's index (`plans/PI.md` P11), host-tested with a
recording sink. Key repeat remains a higher-layer concern.

### Boot-keyboard driver-process orchestration

The `service` module composes the **user-space** boot-keyboard driver
(`plans/PI.md` P10 chunk 5d-2-ii). It is arch-neutral: the board PCIe
root-complex bring-up and BAR assignment stay in the board bus driver
(`drivers/bus/pcie_brcm` + `drivers/bus/usb`); the keyboard driver is
autoloaded against the discovered HID node and granted only its matched
node's resources — its already-assigned xHCI register BAR and a DMA
constraint (`AGENTS.md` §18.3) — reached through the `DriverHost` its
runtime builds over those grants, so the orchestration names no PCI, no
BCM2711, no board (`AGENTS.md` §2.20). `bring_up_boot_keyboard` carves and
aperture-checks the DMA region before touching any register (fail closed),
maps the granted BAR, brings the controller up (`rustos_usb::Xhci::open` +
`UsbDevice::start`, carving the shared `rustos_usb::XHCI_DMA_BYTES`), runs
the arch-neutral `UsbDevice::enumerate_boot_keyboard` (descending an onboard
hub when present), and returns a `BootKeyboard` the service loop drives with
`pump_once`. QEMU models no Pi USB, so the host tests prove the composition
and its fail-closed paths up to the controller hand-off (an inert window
faults `DeviceFault`, the metal boundary); the autoloaded driver binary and
the boot wiring are the next chunk.

## Autoload bind table

`BIND_KEYS` declares two class-wildcard entries — an HID **boot
keyboard** (`0x030101`) and an HID **boot mouse** (`0x030102`), each
binding any vendor/product at that class (`HwMatchKey::matches`) — so any
HID boot device enumerated behind a USB host autoloads this driver
without its device id being hard-coded (`AGENTS.md` §2.2 / §18.3). It is
the single source of truth a signed-manifest bind table is authored from;
`devmgr` resolves the discovered HID node against it once the USB
enumeration emits that node into the tree (`PLAN.md` Stage 4.HW item 5).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `Input::poll` is gated by ownership of the `DriverHandle` returned
  from `register`; the `Input` trait declares no additional per-method
  capability.

The driver holds no ambient authority: it can only consume the report
stream the host-supplied `ReportSource` exposes (`AGENTS.md` §4). It
runs in user space; it does **not** request `CAP_DRV_KERNEL`.

## Lifecycle

`register` clears the load-time gate; `BootKeyboard::new` /
`BootMouse::new` bind a decoder to a report stream without performing
any I/O; dropping the instance releases the source (the unload step).
Reloading is constructing a fresh instance over the endpoint, which the
`unload_then_reload_decodes_again` test exercises.

## Test surface

`cargo test -p rustos-drv-input-usb-hid` exercises, against an
in-process mock report queue:

- `register` capability gate.
- Keyboard: press/release edges, one edge per held key, modifier
  edges, rollover handling, duplicate-usage hostile reports, short
  reports rejected (`LengthOutOfRange`), forged source lengths and
  transport faults rejected (`DeviceFault`), event latching across
  undersized `poll` buffers, and the per-poll report budget.
- Mouse: button diff, X/Y/wheel deltas, 3-byte (wheel-less) reports,
  device-specific button bits and trailing bytes ignored, short
  reports rejected.
- Console producer (`console` module): US-layout letters/digits/shifted
  symbols, caps lock (letters only) and num lock (keypad), the held
  modifiers (shift, `Ctrl-C` → `0x03`), named/editing/arrow/function
  sequences, releases and non-key events producing nothing, unknown
  usages and undersized buffers failing closed, and the full
  decode→keymap→sink chain through `pump_once`.
- Boot-keyboard orchestration (`service` module): the missing
  `CAP_MMIO_MAP`, absent mapper, and absent DMA-host refusals; a DMA carve
  above the inbound aperture and a DMA-allocation failure rejected fail
  closed; and the all-valid path reaching the controller hand-off, where
  the inert mock window faults `DeviceFault` (the metal boundary).
