# rustos-input

Shared input-event vocabulary for the RustOS desktop (`lib/input`,
`AGENTS.md` §6 / §17.4 — `PLAN.md` Stage 7).

This crate owns the device-level input types the desktop routes:

- `PointerButton` — the primary / secondary / middle buttons the desktop
  distinguishes.
- `Modifiers` / `NamedKey` / `Key` — the keyboard vocabulary: the held
  modifier keys, the named non-character keys (Enter, the arrows, F1–F12, …),
  and a `Key` that is either a produced `Char` or a `NamedKey`.
- `InputEvent` — what a device reports: the pointer's `PointerMoved`,
  `PointerPressed`, `PointerReleased` (button events act at the pointer's
  current position, which a router tracks from the motion events) and the
  keyboard's `KeyPressed` / `KeyReleased`, delivered to the focused surface.

## Where it sits

These types were defined inside `userland/gui/wm`, but the taskbar must route
the **same** pointer events to hit-test its regions, and a `userland/gui/*`
crate may not depend on the window manager nor on a sibling userland crate
(`AGENTS.md` §17.4). Per §6 / §2.2 the shared vocabulary therefore lives in
`lib/*` — the same reasoning that placed `Point`/`Rect` in `lib/geometry` and
the colour algebra in `lib/raster`. It is `no_std`, `#![forbid(unsafe_code)]`,
and depends only on `lib/geometry` (a motion event names a screen `Point`). It
is depended on by the GUI crates, never the reverse — `Layer::Lib` in the
§17.4 layering.

Keyboard input is modelled alongside the pointer; this is the in-process
routing vocabulary, while the bytes that cross the kernel boundary are
`rustos_abi`'s `KeyInput` (the same producer/consumer split as `PointerButton`
vs `rustos_abi`'s `PointerButtonCode`).

## Stability

Tier: `experimental`.
