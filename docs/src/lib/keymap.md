# `rustos-keymap`

The shared **terminal key map** for RustOS console-input producers
(`plans/PI.md` P11). A directly attached keyboard driver decodes its device's
scancodes into the `rustos_input::Key` vocabulary, then has to turn each key
press into the byte sequence a terminal sends down its input stream — a
printable character as itself, `Ctrl-C` as the `0x03` control code, the up
arrow as `ESC [ A`. That translation is identical for every keyboard
regardless of how the device delivered the key, so it lives here once rather
than being re-derived in each driver (`AGENTS.md` §2.2).

Stability tier: **experimental**.

## What it defines

| Item | Contents |
|------|----------|
| `encode_key(key, modifiers, out)` | Write the console (tty) bytes one key press sends into `out`, returning their length. |
| `MAX_KEY_BYTES` | The longest sequence any key encodes to; a buffer of this size never overflows. |
| `KeymapError` | The single failure mode — `BufferTooSmall`. |

## Mapping

- A printable `Key::Char` is sent as its UTF-8 bytes. Its case already reflects
  the shift / caps-lock state the layout applied; this map only adds the
  control-key arithmetic: with `Ctrl` held a character becomes its C0 control
  code (`Ctrl-A`..`Ctrl-Z` → `0x01`..`0x1A`, `Ctrl-@`..`Ctrl-_` →
  `0x00`..`0x1F`, `Ctrl-?` → `0x7F`).
- With `Alt`/meta held, a printable character is prefixed with `ESC` (the
  xterm "meta sends escape" convention).
- `Enter` → `CR`, `Tab` → `HT`, `Backspace` → `DEL` (`0x7F`), `Escape` → `ESC`.
- The arrow keys send `ESC [ A`..`ESC [ D`.
- The editing / navigation keys (`Home`, `End`, `Insert`, `Delete`,
  `PageUp`/`PageDown`) and `F1`..`F12` send the canonical `SS3` / `CSI … ~`
  sequences.

## One escape vocabulary

The named-key escape sequences are **not** redefined here: the `SS3` / `CSI …
~` forms come from `rustos_vt::key` and the control bytes from
`rustos_vt::control`, the one canonical ANSI / VT / xterm definition
(`AGENTS.md` §2.2). Only those `const` tables are touched, so the crate is
allocation-free.

## Allocation-free and fail-closed

`encode_key` writes into a caller buffer and never allocates, so it runs in a
driver process before the userland heap is available (`plans/SPAWN.md` SP5b),
and it never panics (`AGENTS.md` §2.9): a too-small buffer is a
`KeymapError::BufferTooSmall` and a key with no terminal encoding produces
nothing rather than guessing. It is an encoder of already-typed input, not a
parser of untrusted bytes.

## Layering and testing

`lib/keymap` depends on `lib/*` only (`rustos_input` for the key vocabulary,
`rustos_vt` for the escape tables) — never on `kernel/*`, `drivers/*`, or
`userland/*` (`AGENTS.md` §17.4) — and is text-mode infrastructure outside
`userland/gui/*`, so a headless image links it freely (§17.3). Its consumer is
the keyboard drivers' console-input producer (`drivers/input/usb_hid`'s
`console` module); the HID-usage→`Key` resolution is the driver's
device-specific half, while this `Key`→bytes map is the shared half. Unit
tests (`AGENTS.md` §7) live next to the code (`src/lib.rs`): the printable,
control, alt-prefix, arrow, editing, and function-key encodings, the
fail-closed buffer and unmappable-key cases, and a `MAX_KEY_BYTES` bound check
over every named key.
