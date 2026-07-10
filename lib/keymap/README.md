# rustos-keymap

Shared terminal key map for RustOS console-input producers (`lib/keymap`,
`AGENTS.md` §6 / §2.2 — `plans/PI.md` P11).

A directly attached keyboard driver (`drivers/input/usb_kbd`,
`drivers/input/ps2`) decodes its device's scancodes into the `rustos_input::Key`
vocabulary, then must turn each key press into the byte sequence a terminal
sends down its input stream — a printable character as itself, `Ctrl-C` as
`0x03`, the up arrow as `ESC [ A`. That translation is the **terminal key
map**, and it is identical for every keyboard regardless of how the device
delivered the key, so it lives here once instead of being re-derived in each
driver (§2.2). The driver feeds the resulting bytes to the kernel through the
`console_input` syscall (`plans/PI.md` P11), which delivers them to the video
console's read half.

## API

- `encode_key(key, modifiers, out) -> Result<usize, KeymapError>` — write the
  console (tty) bytes for one key press into `out`, returning their length.
- `MAX_KEY_BYTES` — the longest sequence any key encodes to; a buffer of this
  size can never overflow.

## Design

- `no_std`, `#![forbid(unsafe_code)]`, **allocation-free** — it writes into a
  caller buffer, so it works in a driver process before the userland heap is
  available (`plans/SPAWN.md` SP5b).
- Fail-closed (§2.9): a too-small buffer is an error and a key with no terminal
  encoding produces nothing rather than guessing. It is an encoder of
  already-typed input, not a parser of untrusted bytes.
- The escape sequences are **not** redefined here: the named-key `SS3` / `CSI …
  ~` forms and the control bytes come from `lib/vt` (`rustos_vt::key`,
  `rustos_vt::control`), the one canonical ANSI / VT / xterm definition (§2.2).
  Only `lib/vt`'s `const` tables are touched, so this crate stays
  allocation-free.

## Stability

Tier: `experimental`.
