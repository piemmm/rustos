# Input events (`abi-v1`)

The desktop is driven by pointer and keyboard input. A device's reports
reach the user-space desktop as a stream of framed records over a
capability-checked kernel input channel; the contracts of those records
live in `lib/abi/src/input.rs` (`rustos_abi::input`).

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

## The keyboard record

[`KeyInput`] is the desktop-level counterpart of `PointerInput`: a key
going down or coming up, the [`KeyValue`] it produced — a Unicode
character (`Char`) or a named
non-character key ([`NamedKeyCode`]: Enter, the arrows, F1–F12, …) — and
the [`Modifiers`] (shift / ctrl / alt / meta) held at the time. As with
the pointer, this is the *resolved* event, not the device report:
turning raw keycodes and a keyboard layout into a produced character is
policy above the driver, not a second copy of the data (`AGENTS.md`
§2.2).

A record is exactly [`KeyInput::WIRE_LEN`] (20) bytes, little-endian: a
`"KIN1"` magic, the ABI version, a `kind` code (pressed / released), the
modifier bitmask, a `key_class` (char / named), a 4-byte codepoint, a
2-byte named-key code, and a reserved half-word. Exactly one of the two
key fields is set for a given class; [`KeyInput::from_bytes`] validates
every field — magic, version, reserved, `kind`, `key_class`, the
modifier bits, the named-key code, and that the codepoint is a real
Unicode scalar (an unpaired surrogate is refused) — and fails closed with
the matching [`Errno`] (`AGENTS.md` §5.4 / §19.5). It too is enrolled in
the `lib/abi` fuzz harness (`AGENTS.md` §19.6).

## Where it is consumed

The desktop session backs its
[`InputSource`](../desktop/session.md) seam with two decoders that share
the same `lib/input` `InputEvent` stream: `DeviceInputSource` reads
`PointerInput` records from the kernel pointer channel, and
`KeyboardInputSource` reads `KeyInput` records from the kernel keyboard
channel. The window manager delivers a decoded key event to the
focused window; the taskbar takes no keyboard input.

[`PointerInput`]: ../../rustos_abi/input/enum.PointerInput.html
[`KeyInput`]: ../../rustos_abi/input/enum.KeyInput.html
[`KeyInput::WIRE_LEN`]: ../../rustos_abi/input/enum.KeyInput.html#associatedconstant.WIRE_LEN
[`KeyInput::from_bytes`]: ../../rustos_abi/input/enum.KeyInput.html#method.from_bytes
[`KeyValue`]: ../../rustos_abi/input/enum.KeyValue.html
[`NamedKeyCode`]: ../../rustos_abi/input/enum.NamedKeyCode.html
[`Modifiers`]: ../../rustos_abi/input/struct.Modifiers.html
[`PointerInput::WIRE_LEN`]: ../../rustos_abi/input/enum.PointerInput.html#associatedconstant.WIRE_LEN
[`PointerInput::from_bytes`]: ../../rustos_abi/input/enum.PointerInput.html#method.from_bytes
[`PointerButtonCode`]: ../../rustos_abi/input/enum.PointerButtonCode.html
[`Errno`]: ../../rustos_abi/error/enum.Errno.html
[`driver::input::InputEvent`]: ../../rustos_abi/driver/input/struct.InputEvent.html
[`Input`]: ../../rustos_abi/driver/input/trait.Input.html
