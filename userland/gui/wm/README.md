# rustos-wm

The RustOS compositing window manager (`userland/gui/wm`, `AGENTS.md`
§10). It composes per-window surfaces into a single scan-out frame and
presents it through a capability-gated
`rustos_abi::driver::display::Display` driver. All compositing happens in
user space; no non-GUI crate depends on this crate (`AGENTS.md` §17.3).

## Status

First Stage 7 increments — the **compositor core** and the **input
router**:

- Premultiplied-alpha pixels (`color`) with the Porter–Duff *over*
  operator: correct per-surface and per-region transparency.
- Surfaces (`surface`): dense premultiplied pixel buffers.
- Anti-aliased rounded corners (`corner`) via deterministic
  supersampling, with a square-corner opt-out — the single
  rounded-corner path the taskbar reuses (`AGENTS.md` §2.2).
- Damage tracking (`damage`): only changed pixels are recomposited.
- The `Compositor`: a z-ordered window stack composited over an opaque
  background into a `DisplayMode`-shaped byte frame, presented through a
  `Display` seam.
- Input routing (`input`): the `InputRouter` tracks the pointer and the
  focused window, raises and focuses the window under a primary press
  (click-to-activate), and drives explicit interactive window
  move-grabs; `Compositor::window_at` is the top-most hit-test. The
  device-level `PointerButton`/`InputEvent` vocabulary it consumes lives
  in the shared `rustos-input` crate (re-exported here) so the taskbar
  routes the same events without depending on the window manager
  (`AGENTS.md` §17.4).
- Pointer cursor overlay (`cursor`): a scalable, colourful, replaceable
  `rustos_cursor::CursorImage` composited as the top-most layer so its
  hotspot tracks the pointer (`AGENTS.md` §2.2 / §2.4).
- Cursor selection (`select`): `desired_cursor` chooses the
  `rustos_theme::CursorKind` from live interaction state — a window
  move-grab shows the move cursor, otherwise the pointer takes the
  per-window `cursor_hint` of the window under it (set with
  `Compositor::set_window_cursor`), and the desktop background shows the
  plain arrow. `CursorController` owns the active `CursorRegistry`, applies
  that policy, and rasterises the chosen artwork through
  `Compositor::set_cursor`, failing closed if a cursor cannot be rasterised.
  It does **not** own the scale: the desktop density belongs to the output, so
  it reads `Compositor::scale` when it rasterises, and `refresh` re-renders the
  pointer when the kind, the cursor set, **or** the output scale changes
  (`AGENTS.md` §10 / §2.2). It rasterises each `CursorKind` at most once per
  scale and cursor set: a shared `rustos-raster` `RasterCache` keyed by kind
  keeps the converted image so re-showing a kind reuses it and only a scale or
  set change re-rasterises (the SVG-first "convert once" rule, `AGENTS.md`
  §10) — the same cache the taskbar uses for its notification glyphs
  (`AGENTS.md` §2.2).

  The compositor owns its output's density (`Compositor::scale` /
  `set_scale`); a window's effective density is `Compositor::window_scale`,
  the read-only query apps use. A multi-monitor desktop is a set of such
  outputs, each carrying its own scale.

GPU acceleration and the default apps build on this core in later
Stage 7 increments.

## Properties

- `no_std` (+ `alloc`); depends only on the shared `lib/*` crates
  (`rustos-abi`, `rustos-raster`, `rustos-geometry`, `rustos-input`,
  `rustos-theme`, `rustos-cursor`) — never on a sibling userland crate
  (`AGENTS.md` §17.4).
- No `unsafe`; no `unwrap`/`expect`/`panic!` in production paths — every
  fallible entry point returns a `Result`/`Option` (`AGENTS.md` §2.9).

## Tests

```
cargo test -p rustos-wm
```

Headless tests against a virtual framebuffer cover premultiplied-alpha
correctness (opaque and transparent edge cases), per-region alpha
blending, rounded-corner masking, z-order, window move/hide/remove with
damage repaint, channel-order encoding, the present seam, and input
routing (hit-testing, click-to-activate focus and raise,
desktop-clears-focus, move-grab drag, and the fail-closed grab edge
cases), the cursor overlay (compositing above windows, move/hide repaint
with damage), and cursor selection (the move-grab/window-hint/desktop
policy, controller shape switching, re-rendering on scale and
cursor-set changes, and reuse of a cached kind when it recurs).
