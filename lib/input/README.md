# rustos-input

Shared pointer input-event vocabulary for the RustOS desktop (`lib/input`,
`AGENTS.md` §6 / §17.4 — `PLAN.md` Stage 7).

This crate owns the device-level pointer types the desktop routes:

- `PointerButton` — the primary / secondary / middle buttons the desktop
  distinguishes.
- `InputEvent` — what a pointing device reports: `PointerMoved`,
  `PointerPressed`, `PointerReleased`. Button events act at the pointer's
  current position; a router tracks that position from the motion events.

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

Keyboard input is deliberately **not** modelled here: the desktop tracks
*which* surface owns the keyboard, but the key encoding is a separate ABI
concern that is not invented in this layer (`AGENTS.md` §2.4).

## Stability

Tier: `experimental`.
