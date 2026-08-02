# tairix-wm

The TAIRiX compositing window manager (`userland/gui/wm`, `AGENTS.md`
§10). It composes per-window surfaces into a single scan-out frame and
presents it through a capability-gated
`tairix_abi::driver::display::Display` driver. All compositing happens in
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
  `Compositor::composite` returns the `DamageRegion` it actually
  recomposited (screen-clipped), and `has_damage` answers exactly what
  that next composite would produce, so a wake loop can skip a frame
  outright. An update that changes nothing marks nothing: a `move_window`
  to the origin the window already has, `set_corners`/`set_visible`/
  `set_opacity` to the current value, still return `true` (only an unknown
  id returns `false`) but repaint no pixel. A content edit reports its own
  damage — `edit_window_surface` takes an edit returning `(value, Rect)`
  in content-local pixels, translated into the window's client rectangle
  and clipped to it — because only the edit, having compared the pixels it
  wrote against the ones already there, knows what truly changed. A
  replaced *surface* is always assumed changed.
- The `Compositor`: a z-ordered window stack composited over an opaque
  background into a `DisplayMode`-shaped byte frame, presented through a
  `Display` seam. `present` composites and then moves only what changed:
  **no damage means no driver call at all** (a wake that changed nothing
  costs neither a scan-out copy nor a blit), whole-screen damage is one
  `Display::present`, and anything else is one `Display::present_region`
  per disjoint dirty rectangle — up to `MAX_PRESENT_REGIONS`, past which a
  single bounding-box present costs less than the round trips it replaces.
  Recomposition resolves each covering layer's source row, the back-buffer
  row, and the frame row once per row (`Window::row`), leaving a column a
  slice index and a blend.
- Input routing (`input`): the `InputRouter` tracks the pointer and the
  focused window, raises and focuses the window under a primary press
  (click-to-activate), and drives explicit interactive window
  move-grabs; `Compositor::window_at` is the top-most hit-test. Keyboard
  focus can also be moved programmatically with `focus(window, &Compositor)`
  (validated against the compositor, fail-closed) and dropped with `unfocus`,
  so the session glue's taskbar can activate a window by id without a pointer
  press. A `KeyPressed`/`KeyReleased` event is delivered to the focused window
  as an `InputResponse::Key` (a key with no focused window, or one whose
  window has since gone, is ignored and the stale focus dropped, `AGENTS.md`
  §2.9). The device-level `PointerButton`/`InputEvent` vocabulary it consumes —
  including the `Key`/`NamedKey`/`Modifiers` keyboard types — lives in the
  shared `tairix-input` crate (re-exported here) so the taskbar routes the same
  events without depending on the window manager (`AGENTS.md` §17.4).
- Pointer cursor overlay (`cursor`): a scalable, colourful, replaceable
  `tairix_cursor::CursorImage` composited as the top-most layer so its
  hotspot tracks the pointer (`AGENTS.md` §2.2 / §2.4). Cursor damage is
  derived at composite time from the footprint the *last* composite drew,
  so it is that rectangle plus the one the cursor now occupies — a whole
  batch of pointer samples pumped between two composites costs two
  rectangles, not one per sample, because no intermediate position was
  ever drawn. Replacement artwork always repaints even on an identical
  rectangle (the pointer picking up a text or resize shape without
  moving), and a move that lands where the cursor already is repaints
  nothing.
- Cursor selection (`select`): `desired_cursor` chooses the
  `tairix_theme::CursorKind` from live interaction state — a window
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
  scale and cursor set: a `tairix-reclaim` `ReclaimCache` keyed by kind keeps
  the converted image so re-showing a kind reuses it and only a scale or set
  change re-rasterises (the SVG-first "convert once" rule, `AGENTS.md` §10) —
  a bounded, pressure-governed cache built from the shared desktop cache
  policy (`plans/SMARTRAM.md` SMART5), the same policy the taskbar uses
  for its notification glyphs (`AGENTS.md` §2.2). `CursorController` never
  builds this cache itself: `cursor_cache` assembles it from the real output
  size, the owning seat, and the process's live pressure gauge and audit
  sink, and the caller hands the result to `new` or `with_registry` as a
  required constructor argument — there is no parameterless fallback,
  because a cache built without a live gauge would classify and serve every
  lookup correctly while retaining nothing.

  The compositor owns its output's density (`Compositor::scale` /
  `set_scale`); a window's effective density is `Compositor::window_scale`,
  the read-only query apps use. A multi-monitor desktop is a set of such
  outputs, each carrying its own scale.

GPU acceleration and the default apps build on this core in later
Stage 7 increments.

## Properties

- `no_std` (+ `alloc`); depends only on the shared `lib/*` crates
  (`tairix-abi`, `tairix-raster`, `tairix-geometry`, `tairix-input`,
  `tairix-theme`, `tairix-cursor`, `tairix-reclaim`) — never on a sibling
  userland crate (`AGENTS.md` §17.4).
- No `unsafe`; no `unwrap`/`expect`/`panic!` in production paths — every
  fallible entry point returns a `Result`/`Option` (`AGENTS.md` §2.9).

## Tests

```
cargo test -p tairix-wm
```

Headless tests against a virtual framebuffer cover premultiplied-alpha
correctness (opaque and transparent edge cases), per-region alpha
blending, rounded-corner masking, z-order, window move/hide/remove with
damage repaint, channel-order encoding, the present seam (no damage
presents nothing, disjoint rectangles present individually, whole-screen
damage presents once, and more rectangles than the limit collapse to one
bounding-box present), no-op updates marking nothing, edit-reported
content damage (offset by a decoration band, clipped to the client, empty
marking nothing), and input routing (hit-testing, click-to-activate focus
and raise, desktop-clears-focus, programmatic `focus`/`unfocus` with the
fail-closed unknown-window path, move-grab drag, and the fail-closed grab
edge cases), the cursor overlay (compositing above windows, move/hide
repaint with damage, a multi-sample sweep composing the byte-identical
frame a single move to the same place does), and cursor selection (the
move-grab/window-hint/desktop policy, controller shape switching,
re-rendering on scale and cursor-set changes, and reuse of a cached kind
when it recurs).
