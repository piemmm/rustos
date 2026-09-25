# `tairix-hid`

`lib/hid` is the arch-neutral, transport-agnostic HID logic the USB-HID
keyboard/mouse drivers are built from: the report decoders, the Report
Descriptor parser and normaliser, and the console-input producer. It is
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
  and emits one `Key` edge per change: a usage repeated across slots is pressed
  and released once, and the modifier bitmap alone reports the modifiers — a
  modifier usage in the key array is ignored, or the one modifier would be
  pressed twice. Everything fails closed (wrong-length reports rejected whole,
  a forged length is a `DeviceFault`, overflowing events are latched not
  dropped, a per-`poll` budget bounds a flooding device, `AGENTS.md` §5.4 /
  §2.1).
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
  does declare IDs, is undemuxable and refuses the whole map. A kind's fields
  are taken only from the report its first field was — a sibling collection's
  fields are another report's bits, and reading them fabricated motion out of a
  click — and each report's field offsets run on across the items of other
  reports interleaved with it; Push and Pop save and restore the Report ID with
  the rest of the global state (USB HID 1.11 §6.2.2.7), and a long item is
  skipped whole. A map locating a field the boot layout cannot read — a button
  or modifier bitmap not one bit per flag, a value wider than 32 bits, a field
  of no elements — is refused rather than accepted as a map that drops every
  report; each bitmap is read at its own width, at most the boot byte's eight
  bits. An axis is found by its usage's index, so the work stays the
  descriptor's length whatever `Report Count` it declares. `normalize` is
  otherwise fail-soft — a report captured a byte
  short (a longer report clipped to the capture buffer) still delivers the
  fields that arrived rather than dropping the whole report, which had silenced
  every keypress on a Report-ID keyboard. Pure, `no_std`, alloc-free;
  an undecodable or unsupported descriptor yields `None` (the caller falls back
  to boot protocol), never a guess or a panic (`AGENTS.md` §2.9).
  **The decode reports what the device reported, and never conditions it.** No
  debounce, no coalescing, no suppression of a button edge here: this layer has
  no clock and no policy, and a decoder that quietly dropped edges would make
  every consumer's view of the device unknowable. Chatter filtering is the
  *seat's*, applied once at the one funnel every pointer injector passes through
  and settled by the operator (`input.mouse.debounce`,
  `docs/src/desktop/seat.md`) — not per driver, and never in the decode.

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

## Layering and platform-neutrality

`lib/hid` depends only on other `lib/*` crates — `lib/abi` (the input/event and
`ReportSource` surface), `lib/input` (the `Key` vocabulary), and `lib/keymap`
(the `Key`→record map) — so it satisfies §17.4 and names no board, PCI, or SoC
detail (`AGENTS.md` §2.20). It touches no register and holds no DMA: controller
bring-up and enumeration are the host-controller driver's
(`drivers/bus/usb/xhci`, over `tairix-usb`), which binds the controller node,
while the class drivers bind the HID interface nodes it publishes by their
class.

## Test surface

`cargo test -p tairix-hid` exercises, against an in-process mock report queue:

- Keyboard decode: press/release edges, one edge per held key, modifier edges,
  rollover handling, duplicate-usage hostile reports pressed and released once,
  modifier usages in the key array left to the bitmap, short reports rejected,
  forged source lengths and transport faults rejected, event latching across
  undersized buffers, and the per-`poll` report budget.
- Mouse decode: button diff, X/Y/wheel deltas, 3-byte (wheel-less) reports,
  device-specific button bits and trailing bytes ignored, short reports
  rejected.
- Report-descriptor parse + normalise: the canonical boot mouse and keyboard
  Report Descriptors parse to the right field layout; a report-protocol report
  normalises to the boot bytes (idle no-op, wheel, 12-bit axes clamped to
  `i8`, buttons past the boot byte's eight); a Report-ID-prefixed report demuxes
  by ID; a foreign ID, a truncated report, or a too-small output buffer fails
  closed; junk/empty/oversize descriptors are rejected. A long item is skipped
  whole; a field of another report is never read from this one's; a
  re-entered report's offsets continue; a Pop restores the Report ID; a map
  locating an unreadable field is refused; a narrow modifier field yields only
  its own flags; an axis item of `2^32 - 1` fields is located by its usages.
- Console producer: US-layout letters/digits/shifted symbols, caps/num lock,
  the held modifiers, named/editing/arrow/function sequences, releases and
  non-key events producing nothing, and the full decode→keymap→sink chain
  through `pump_once`.
- Pump-loop error policy: only a vanished endpoint reads as the transport
  disappearing; a register the build cannot decode (including `i64::MIN`, whose
  negation would abort the process) reads as a device fault rather than a
  removed device; and the consecutive-failure counter saturates instead of
  wrapping back under its limit.

The parser, the normaliser, and both boot decoders are fuzzed (`fuzz_hid_report`,
run by `cargo xtask fuzz`) against a naive model of each: an accepted map's
fields readable, clear of the Report ID byte and of each other; every
normalised report exactly what a naive bit reader takes; a long item, or
globals a Push and Pop enclose, changing nothing; and every decoded edge
changing the state it reports.

## Stability

Tier: `experimental` (see the crate `README.md`).
