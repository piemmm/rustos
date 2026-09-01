# `tairix-hid`

`lib/hid` is the arch-neutral, transport-agnostic HID boot-protocol logic the
USB-HID keyboard/mouse driver is built from: the report decoders, the
console-input producer, and the xHCI boot-keyboard orchestration. It is
**generic** HID-protocol code (it names no device, board, PCI id, or SoC), so
it lives in `lib/*` as shared common code (`AGENTS.md` §6 / §2.2) — *not*
under the §2.20 / §2.22 single-device carve-out. The user-space keyboard
and mouse class-driver processes (`drivers/input/usb_kbd`,
`drivers/input/usb_mouse`) compose it without a `drivers/*`→`drivers/*`
dependency (`AGENTS.md` §17.4 /
§2.2), exactly as the bus-agnostic xHCI protocol lives in
[`tairix-usb`](./usb.md) rather than the xHCI driver.

## What it provides

- **Decoders** (`BootKeyboard`, `BootMouse`): the fixed 8-byte keyboard report
  and the 3-or-more-byte mouse report (USB HID 1.11 Appendix B) decoded into
  platform-neutral `tairix_abi::driver::input::InputEvent`s. The decoders are
  written against the `ReportSource` seam (defined in `lib/abi`, because its
  producer is the xHCI driver), so they are proven host-side over a mock report
  queue while the transport below them is proven on metal (`AGENTS.md` §2.2).
  The keyboard report carries state, so the decoder diffs consecutive reports
  and emits one `Key` edge per change; everything fails closed (wrong-length
  reports rejected whole, a forged length is a `DeviceFault`, overflowing
  events are latched not dropped, a per-`poll` budget bounds a flooding device,
  `AGENTS.md` §5.4 / §2.1).
- **Report-descriptor parser + boot-layout normaliser** (`report`:
  `parse_report_descriptor` → `HidReportMap`, `HidReportMap::normalize`): a
  fail-closed HID Report Descriptor parser (USB HID 1.11 §6.2.2) that locates
  the boot fields (mouse buttons/X/Y/wheel, keyboard modifiers/key-array)
  inside a **report-protocol** report, and a normaliser that rewrites one such
  report back into the fixed boot layout the decoders above consume. The HID
  enumeration engine (`tairix-usb`) uses it to run a device in report protocol
  — the mode in which `SET_IDLE` quiesces an idle device that would otherwise
  stream a duplicate report every polling interval — while the class drivers
  and the URB ABI keep seeing boot-format reports. It handles a device that
  declares a **Report ID** (reports carry a leading ID byte, so the boot fields
  sit one byte later). That id is an `Option`, never `0`-as-"none": a device
  with no Report IDs needs no demux, while one with them must have every report
  *matched*, and spelling the first as id `0` made `normalize` skip the demux
  entirely — normalising every sibling collection's report as this interface's
  own, its ID byte landing where the button bitmap is read from. A boot field
  located before the descriptor's first Report ID item, in a descriptor that
  does declare IDs, is undemuxable and refuses the whole map. `normalize` is
  otherwise fail-soft — a report captured a byte
  short (a longer report clipped to the capture buffer) still delivers the
  fields that arrived rather than dropping the whole report, which had silenced
  every keypress on a Report-ID keyboard. Pure, `no_std`, alloc-free;
  an undecodable or unsupported descriptor yields `None` (the caller falls back
  to boot protocol), never a guess or a panic (`AGENTS.md` §2.9).
  `HidReportMap::summary` → `ReportMapSummary` exposes what the parser decided
  — one variant per device kind, naming every located field's bit offset, width,
  and element count — so the xHCI driver can log how a device's reports are
  being read on metal. A shared primary/secondary pair could not report a
  pointer's Y axis or its wheel, and those are the offsets that show whether
  button bits are read from the right place.
- **Console-input producer** (`KeyboardConsole`, `pump_once`, `ConsoleSink`):
  resolves each HID-usage key edge into the `tairix_input::Key` a US layout
  produces (applying held modifiers + caps/num lock) and emits the decoded
  `tairix_abi::input::KeyInput` record through the shared `lib/keymap` map — the
  one definition of the `Key`→record translation (`AGENTS.md` §2.2). A driver
  loop (`pump_once`) injects each record through a `ConsoleSink`; the kernel
  input-focus arbiter decides the encoding and destination (`AGENTS.md` §17.4).
- **Pump-loop error policy** (`transport_error`, `pump_error_limit_reached`):
  the one classification every boot-protocol driver's service loop shares.
  Only `Errno::NotFound` — the transport endpoint itself gone, so the host
  controller retracted the interface — becomes `DriverError::NotFound`, which a
  pump loop reads as a clean unplug and exits on. Every other refusal,
  including a register this build cannot decode, is a `DriverError::DeviceFault`
  the driver reports concretely and rides out under the saturating
  consecutive-failure limit before failing closed (`AGENTS.md` §2.2, §5.4). An
  unreadable refusal must not be able to pass itself off as a removed device,
  and the counter saturates so a long-running driver cannot wrap it back under
  the limit and retry for ever.
- **Boot-keyboard orchestration** (`bring_up_boot_keyboard`,
  `derive_keyboard_resources`, `KeyboardResources`): the composition a
  user-space keyboard driver runs at start-up. Over its
  `tairix_abi::DriverHost` it carves the device-shared DMA region (aperture
  checked before any register is touched, `AGENTS.md` §5.4), maps the granted
  xHCI register BAR, brings the controller up over `tairix-usb`, and enumerates
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
assignment stay in the board bus drivers (`drivers/bus/pcie_brcm` +
`drivers/bus/usb`); `lib/hid` maps a register window by address and carves a
DMA region by constraint.

## Test surface

`cargo test -p tairix-hid` exercises, against an in-process mock report queue
and mock `DriverHost`:

- Keyboard decode: press/release edges, one edge per held key, modifier edges,
  rollover handling, duplicate-usage hostile reports, short reports rejected,
  forged source lengths and transport faults rejected, event latching across
  undersized buffers, and the per-`poll` report budget.
- Mouse decode: button diff, X/Y/wheel deltas, 3-byte (wheel-less) reports,
  device-specific button bits and trailing bytes ignored, short reports
  rejected.
- Report-descriptor parse + normalise: the canonical boot mouse and keyboard
  Report Descriptors parse to the right field layout; a report-protocol report
  normalises to the boot bytes (idle no-op, wheel, 12-bit axes clamped to
  `i8`); a Report-ID-prefixed report demuxes by ID; a foreign ID, a truncated
  report, or a too-small output buffer fails closed; junk/empty/oversize
  descriptors are rejected.
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
- Pump-loop error policy: only a vanished endpoint reads as the transport
  disappearing; a register the build cannot decode (including `i64::MIN`, whose
  negation would abort the process) reads as a device fault rather than a
  removed device; and the consecutive-failure counter saturates instead of
  wrapping back under its limit.
- Bind table: `KEYBOARD_BIND_KEYS` matches the published xHCI controller node
  (`tairix_usb::XHCI_COMPATIBLE`) and rejects a different `compatible` string.

## Bind table

`KEYBOARD_BIND_KEYS` is the §18.3 bind table for the user-space USB
boot-keyboard driver (`drivers/input/usb_kbd`): an exact `compatible`-string
match on `tairix_usb::XHCI_COMPATIBLE` (`usb,xhci`), the identity the VL805 USB
bus driver publishes the controller node under (`drivers/bus/usb/vl805`'s
`node B`). The keyboard driver brings the whole xHCI controller up itself — the
`Xhci` controller object cannot cross a process boundary — so it binds the
controller node directly rather than a separately-emitted HID-interface node.
The table lives here, beside the orchestration the driver runs, as the single
source the signed manifest is authored from (`AGENTS.md` §2.2 / §18.3).

## Stability

Tier: `experimental` (see the crate `README.md`).
