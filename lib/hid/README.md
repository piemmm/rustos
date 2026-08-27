# `tairix-hid`

Arch-neutral, transport-agnostic HID boot-protocol logic: the keyboard/mouse
report decoders, the console-input producer, and the xHCI boot-keyboard
orchestration. This is **generic** HID-protocol code — it names no device,
board, PCI id, or SoC — so it lives in `lib/*` as shared common code
(`AGENTS.md` §6 / §2.2), *not* under the §2.20 / §2.22 single-device
carve-out. The user-space keyboard and mouse class-driver processes
(`drivers/input/usb_kbd`, `drivers/input/usb_mouse`) compose it without a
`drivers/*`→`drivers/*` dependency (`AGENTS.md` §17.4 / §2.2).

See `docs/src/lib/hid.md` for the full description and test surface.

## Public surface

- `BootKeyboard`, `BootMouse` — boot-protocol report decoders over the
  `tairix_abi::driver::input::ReportSource` seam.
- `KeyboardConsole`, `pump_once`, `ConsoleSink` — the console-input producer
  that resolves HID usages to `KeyInput` records (via `lib/keymap`) and injects
  them through a sink. Held modifiers are tracked over the shared
  `tairix_input::ModifierState`, and a modifier edge that changes the
  *observable* set emits a `KeyInput::ModifiersChanged` record so the desktop
  can qualify a gesture that is not a key (a shift-click); a repeat, or letting
  go of one shift key while the other is held, emits nothing.
- `bring_up_boot_keyboard`, `derive_keyboard_resources`, `KeyboardResources`,
  `KeyboardSource` — the user-space boot-keyboard bring-up over a
  `DriverHost` + the grant→BAR/DMA-aperture derivation.
- `transport_error`, `pump_error_limit_reached` — the pump loop's error
  policy: which refusal means the transport itself has gone (and so a clean
  unplug), and the saturating consecutive-failure limit that fails a wedged
  device closed. Shared by every boot-protocol driver, so an unreadable
  refusal cannot read as a removed device in one driver and a fault in the
  next.
- `AXIS_X`, `AXIS_Y`, `REPORT_BUF_LEN`, `REPORT_POLL_BUDGET`, and the
  `ReportSource` re-export.

## Dependencies

`lib/abi`, `lib/input`, `lib/keymap`, `lib/usb` — all `lib/*` (§17.4). Names no
board, PCI, or SoC detail (`AGENTS.md` §2.20).

## Stability

Tier: `experimental`. The decode/console/orchestration surface is still
evolving alongside the `plans/PI.md` P10 USB-keyboard bring-up; `abi-v1` types
it exchanges are governed by `lib/abi`.

## Tests

`cargo test -p tairix-hid` — decode, console-producer, orchestration, and
grant-derivation unit tests against in-process mocks (`AGENTS.md` §7).
