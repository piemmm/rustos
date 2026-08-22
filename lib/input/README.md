# tairix-input

Shared input-event vocabulary for the TAIRiX desktop (`lib/input`,
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
- `PointerFocus` — the *derived* half: `Entered { at }` / `Left`, the
  enter/leave pair a seat hands to a surface's router. No device produces it;
  the seat resolves it from the window stack, which is the one fact a surface
  cannot see about itself. A surface acts on pointer input only while it holds
  the pointer, and is told when it stops holding it so the hover it is drawing
  goes away with the pointer rather than being stranded under whatever is now
  drawn over it. It is deliberately *not* an `InputEvent` variant: mixing a
  seat's conclusions into the device vocabulary would make every producer of
  device events look like it could reach a conclusion. It is a *message*, not
  state, and carries no `Default`: the seat is the one owner of which surface
  holds the pointer, and a surface keeping its own copy would be a second
  answer that could disagree.

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
`tairix_abi`'s `KeyInput` (the same producer/consumer split as `PointerButton`
vs `tairix_abi`'s `PointerButtonCode`).

## Stability

Tier: `experimental`.
