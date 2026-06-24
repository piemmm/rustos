# `rustos-hid`

Arch-neutral, transport-agnostic HID boot-protocol logic: the keyboard/mouse
report decoders, the console-input producer, and the xHCI boot-keyboard
orchestration. This is **generic** HID-protocol code — it names no device,
board, PCI id, or SoC — so it lives in `lib/*` as shared common code
(`AGENTS.md` §6 / §2.2), *not* under the §2.20 / §2.22 single-device
carve-out. The user-space keyboard driver process (`drivers/input/usb_kbd`)
and the thin `drivers/input/usb_hid` crate (which keeps only the §8 `register`
entry and the §18.3 bind table) both compose it without a
`drivers/*`→`drivers/*` dependency (`AGENTS.md` §17.4 / §2.2).

See `docs/src/lib/hid.md` for the full description and test surface.

## Public surface

- `BootKeyboard`, `BootMouse` — boot-protocol report decoders over the
  `rustos_abi::driver::input::ReportSource` seam.
- `KeyboardConsole`, `pump_once`, `ConsoleSink` — the console-input producer
  that resolves HID usages to `KeyInput` records (via `lib/keymap`) and injects
  them through a sink.
- `bring_up_boot_keyboard`, `derive_keyboard_resources`, `KeyboardResources`,
  `KeyboardSource` — the user-space boot-keyboard bring-up over a
  `DriverHost` + the grant→BAR/DMA-aperture derivation.
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

`cargo test -p rustos-hid` — decode, console-producer, orchestration, and
grant-derivation unit tests against in-process mocks (`AGENTS.md` §7).
