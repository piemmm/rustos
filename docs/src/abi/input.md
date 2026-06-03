# Pointer input events (`abi-v1`)

The desktop is driven by pointer input. A pointing device's reports reach
the user-space desktop as a stream of framed records over a
capability-checked kernel input channel; the contract of one such record
lives in `lib/abi/src/input.rs` (`rustos_abi::input`).

## The record

[`PointerInput`] is one decoded pointer event. The type makes illegal
states unrepresentable (`AGENTS.md` §2.11):

- `Moved { x, y }` — the pointer moved to an absolute screen position.
- `Pressed(button)` / `Released(button)` — a [`PointerButtonCode`]
  (primary / secondary / middle) went down or came up at the current
  pointer position.

A record is exactly [`PointerInput::WIRE_LEN`] (20) bytes, little-endian:
a `"PIN1"` magic, the two-byte ABI version, a `kind` code, a `button`
code, a reserved half-word, and two 4-byte signed coordinates. The
coordinates carry the new position for a move and are zero for a press or
release (a pointing device reports motion separately from clicks, and a
router applies a button at the position the last motion established — the
same model as `lib/input`).

## Fail-closed decoding

[`PointerInput::from_bytes`] validates every field before returning a
value and refuses anything inconsistent rather than guessing
(`AGENTS.md` §5.4 / §19.5): a short buffer, a wrong magic, an
unsupported version, a non-zero reserved field, an undefined `kind`, a
`button` code inconsistent with the kind (a button on a move, or no /
unknown button on a press), or a coordinate on a press/release all fail
with the matching [`Errno`]. The decoder is enrolled in the `lib/abi`
fuzz harness (`AGENTS.md` §19.6).

## Relationship to the driver input ABI

This is **not** a duplicate of the device-level
[`driver::input::InputEvent`] (`AGENTS.md` §2.2). That type is what an
input *driver* reports across the [`Input`] driver trait: raw per-axis
pointer *deltas*, scroll ticks, and platform keycodes. `PointerInput` is
the *desktop-level* event the window manager and taskbar route — an
**absolute** position and a *resolved* button. Turning the
device-relative driver stream into this resolved stream is pointer-input
policy that sits above the driver; the two ABIs are on opposite sides of
it.

## Where it is consumed

The desktop session backs its
[`InputSource`](../desktop/session.md) seam with `DeviceInputSource`,
which reads `PointerInput` records from the kernel input channel and
decodes each into the `lib/input` `InputEvent` the window manager and
taskbar route. Keyboard input is deliberately **not** modelled here: the
desktop tracks *which* surface owns the keyboard, but the key encoding is
a separate ABI concern not invented in this layer (`AGENTS.md` §2.4 — no
interface creep).

[`PointerInput`]: ../../rustos_abi/input/enum.PointerInput.html
[`PointerInput::WIRE_LEN`]: ../../rustos_abi/input/enum.PointerInput.html#associatedconstant.WIRE_LEN
[`PointerInput::from_bytes`]: ../../rustos_abi/input/enum.PointerInput.html#method.from_bytes
[`PointerButtonCode`]: ../../rustos_abi/input/enum.PointerButtonCode.html
[`Errno`]: ../../rustos_abi/error/enum.Errno.html
[`driver::input::InputEvent`]: ../../rustos_abi/driver/input/struct.InputEvent.html
[`Input`]: ../../rustos_abi/driver/input/trait.Input.html
