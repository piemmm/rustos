# `rustos-hid`

`lib/hid` is the arch-neutral, transport-agnostic HID boot-protocol logic the
USB-HID keyboard/mouse driver is built from: the report decoders, the
console-input producer, and the xHCI boot-keyboard orchestration. It lives in
`lib/*` — not in a driver crate — so **both** the transitional in-kernel
keyboard scaffold and the user-space keyboard driver process
(`drivers/input/usb_kbd`) compose it without a `drivers/*`→`drivers/*`
dependency (`AGENTS.md` §17.4 / §2.2), exactly as the bus-agnostic xHCI
protocol lives in [`rustos-usb`](./usb.md) rather than the xHCI driver. The
thin `drivers/input/usb_hid` crate keeps only the §8 `register` entry and the
§18.3 bind table.

## What it provides

- **Decoders** (`BootKeyboard`, `BootMouse`): the fixed 8-byte keyboard report
  and the 3-or-more-byte mouse report (USB HID 1.11 Appendix B) decoded into
  platform-neutral `rustos_abi::driver::input::InputEvent`s. The decoders are
  written against the `ReportSource` seam (defined in `lib/abi`, because its
  producer is the xHCI driver), so they are proven host-side over a mock report
  queue while the transport below them is proven on metal (`AGENTS.md` §2.2).
  The keyboard report carries state, so the decoder diffs consecutive reports
  and emits one `Key` edge per change; everything fails closed (wrong-length
  reports rejected whole, a forged length is a `DeviceFault`, overflowing
  events are latched not dropped, a per-`poll` budget bounds a flooding device,
  `AGENTS.md` §5.4 / §2.1).
- **Console-input producer** (`KeyboardConsole`, `pump_once`, `ConsoleSink`):
  resolves each HID-usage key edge into the `rustos_input::Key` a US layout
  produces (applying held modifiers + caps/num lock) and emits the decoded
  `rustos_abi::input::KeyInput` record through the shared `lib/keymap` map — the
  one definition of the `Key`→record translation (`AGENTS.md` §2.2). A driver
  loop (`pump_once`) injects each record through a `ConsoleSink`; the kernel
  input-focus arbiter decides the encoding and destination (`AGENTS.md` §17.4).
- **Boot-keyboard orchestration** (`bring_up_boot_keyboard`,
  `derive_keyboard_resources`, `KeyboardResources`): the composition a
  user-space keyboard driver runs at start-up. Over its
  `rustos_abi::DriverHost` it carves the device-shared DMA region (aperture
  checked before any register is touched, `AGENTS.md` §5.4), maps the granted
  xHCI register BAR, brings the controller up over `rustos-usb`, and enumerates
  the boot keyboard. `derive_keyboard_resources` turns the kernel-issued
  device-resource grants into the BAR window + DMA-aperture bounds the bring-up
  needs — exactly one register window (an `Mmio` window by base, or an outbound
  `BusWindow` by far-side bus address) and exactly one `Dma` constraint
  (device-visible exclusive top), failing closed on a missing, ambiguous, or
  zero-length grant and never guessing a board constant (`AGENTS.md` §2.16 /
  §2.20 / §5.4).

## Layering and platform-neutrality

`lib/hid` depends only on other `lib/*` crates — `lib/abi` (the input/event,
`ReportSource`, `DriverHost`, and `HwResource` surface), `lib/input` (the `Key`
vocabulary), `lib/keymap` (the `Key`→record map), and `lib/usb` (the
bus-agnostic xHCI protocol) — so it satisfies §17.4 and names no board, PCI, or
SoC detail (`AGENTS.md` §2.20). The board PCIe root-complex bring-up and BAR
assignment stay in the board bus drivers (`lib/pcie_brcm` +
`drivers/bus/usb`); `lib/hid` maps a register window by address and carves a
DMA region by constraint.

## Test surface

`cargo test -p rustos-hid` exercises, against an in-process mock report queue
and mock `DriverHost`:

- Keyboard decode: press/release edges, one edge per held key, modifier edges,
  rollover handling, duplicate-usage hostile reports, short reports rejected,
  forged source lengths and transport faults rejected, event latching across
  undersized buffers, and the per-`poll` report budget.
- Mouse decode: button diff, X/Y/wheel deltas, 3-byte (wheel-less) reports,
  device-specific button bits and trailing bytes ignored, short reports
  rejected.
- Console producer: US-layout letters/digits/shifted symbols, caps/num lock,
  the held modifiers, named/editing/arrow/function sequences, releases and
  non-key events producing nothing, and the full decode→keymap→sink chain
  through `pump_once`.
- Boot-keyboard orchestration: the cap-missing / no-mapper / no-DMA-host
  refusals, a DMA carve above the inbound aperture and a DMA-allocation failure
  rejected fail closed, and the all-valid path reaching the controller hand-off
  (where the inert mock window faults `DeviceFault` — the metal boundary).
- Grant derivation: the Pi 4 shape (`BusWindow` BAR + translated `Dma`) and the
  `virt` shape (`Mmio` BAR + untranslated `Dma`) decode to the right bounds; an
  IRQ grant is ignored; missing / ambiguous / zero-length grants fail closed.
- Bind table: `KEYBOARD_BIND_KEYS` matches the published xHCI controller node
  (`rustos_usb::XHCI_COMPATIBLE`) and rejects a different `compatible` string.

## Bind table

`KEYBOARD_BIND_KEYS` is the §18.3 bind table for the user-space USB
boot-keyboard driver (`drivers/input/usb_kbd`): an exact `compatible`-string
match on `rustos_usb::XHCI_COMPATIBLE` (`usb,xhci`), the identity the VL805 USB
bus driver publishes the controller node under (`drivers/bus/usb/vl805`'s
`node B`). The keyboard driver brings the whole xHCI controller up itself — the
`Xhci` controller object cannot cross a process boundary — so it binds the
controller node directly rather than a separately-emitted HID-interface node.
The table lives here, beside the orchestration the driver runs, as the single
source the signed manifest is authored from (`AGENTS.md` §2.2 / §18.3).

## Stability

Tier: `experimental` (see the crate `README.md`).
