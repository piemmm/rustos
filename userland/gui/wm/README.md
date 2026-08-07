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
- Backdrop blur (`blur`): a window can ask for the already-composited
  content behind its rectangle to be frosted before its own translucent
  pixels blend over it. Composition is back-to-front, so the back buffer
  already holds that backdrop: the compositor composes the layers below
  the window, blurs the back buffer inside that window's rectangle only,
  and resumes composing from the window itself. The filter is a separable
  box blur carrying running sums — cost proportional to the rectangle's
  area whatever the radius — over premultiplied channels including alpha,
  with samples past an edge replicating it, so the effect can neither pull
  a neighbour's pixels in nor write outside the rectangle and a uniform
  backdrop comes out unchanged. The radius is a desktop length in logical
  pixels resolved through the output's `Scale`, and the mix back into the
  back buffer is weighted by the window's own rounded-corner coverage, so
  a rounded window's frosting fades across exactly the arc its pixels do.
  Both scratch buffers belong to the compositor and grow to the largest
  frosted rectangle the session has needed, so a frosted window allocates
  nothing after its first frame. Because those pixels are a function of
  the whole backdrop beneath them, every damage rectangle touching a
  visible frosted window grows to that window's full bounds, iterated
  until nothing grows; and `present_accelerated` takes the software
  fallback outright while any visible window is frosted, because a
  hardware layer is composed from its own pixels and cannot sample what is
  already behind it.
- Damage tracking (`damage`): only changed pixels are recomposited.
  `Compositor::composite` returns the `DamageRegion` it actually
  recomposited (screen-clipped), and `has_damage` answers exactly what
  that next composite would produce, so a wake loop can skip a frame
  outright. An update that changes nothing marks nothing: a `move_window`
  to the origin the window already has, `set_corners`/`set_visible`/
  `set_opacity`/`set_backdrop_blur` to the current value, still return
  `true` (only an unknown id returns `false`) but repaint no pixel. A
  present reports its own damage — `present_window_content` takes the
  presented frame's extent and a conversion returning `(value, Rect)` in
  content-local pixels, translated into the window's client rectangle and
  clipped to it — because only the conversion, having compared the pixels
  it wrote against the ones already there, knows what truly changed. A
  replaced *surface* is always assumed changed.
- The frame is the window manager's; the pixels are the client's. A
  window's content buffer is sized by the frame the **client** presents:
  `resize_window`/`resize_window_client` move the geometry the compositor
  draws and lays furniture out from and never touch the buffer, and
  `present_window_content` establishes it whenever the one held does not
  describe the presented frame. A resize-grab therefore moves the frame on
  every pointer motion while the app, told its new size only once the drag
  settles, keeps presenting the geometry it last knew — accepted, with the
  part that lands inside the client area drawn, instead of refused as it
  would be if the window manager had reshaped the buffer under the app.
  A drag costs no per-motion copy of the window's pixels.
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
- Popup surfaces need no compositor primitive of their own. An app-owned
  popup (`tairix_abi::window_ipc::WindowRequest::CreatePopup`) is an
  ordinary undecorated window in that same flat z-order, placed by the
  session at its clamped screen origin; "directly above its parent" is
  re-asserted with `raise(parent)` then `raise(popup)` once per wake, just
  before `present`, so nothing raised earlier in the frame can land between
  them. The compositor gains no popup concept, no second stacking rule, and
  no parent link.
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
- Window furniture (`chrome`): a decorated window's frame is kept as the
  four strips it actually draws into — the top (title) band, the bottom
  band, and the two side borders — never one surface the size of the whole
  outer window, because the region between them is never sampled (the
  compositor draws the window's content there). Retained bytes therefore
  follow the frame band's thickness rather than the window's area, and a
  screen row needs at most two of the strips at once. The strips live in a
  bounded, pressure-governed `tairix-reclaim` `ReclaimCache` the
  **compositor** owns and every window shares, ceilinged at one screenful
  (no more furniture than fills the screen can be visible at once, so
  everything above that belongs to minimised, off-screen, or stacked-under
  windows — exactly what eviction should take first). Furniture carries the
  window's title, so a released entry is overwritten rather than merely
  dropped. As with the cursor cache, the compositor never builds its own
  policy: `chrome_cache` assembles it from the real output size, the owning
  seat, and the process's live pressure gauge and audit sink, and the caller
  hands the result to `Compositor::new`. A change to one window (title,
  focus, resize, size-state, decorating, closing) releases just that
  window's entry; only a scale or theme change drops the whole cache. The
  cache is an accelerator and never a correctness requirement — anything it
  refuses is rendered for that frame alone, and the composited output is
  byte-identical warm, emptied, or with a budget of zero.
- Releasable window content (`window`): a window's content pixels are the
  desktop's largest single allocation, and a minimised full-screen window
  would otherwise pin them for as long as it exists, so
  `Compositor::release_content_under_pressure` gives them back on the same
  pressure gauge and the same `tairix_reclaim::shrink_target` ordering the
  caches use. It is deliberately a **policy**, not a fourth cache: evicting
  a visible window's pixels is a visual defect rather than a slowdown, so
  what goes is decided by what the user can see — nothing at `Normal`,
  every hidden or minimised window at `Mild` and deeper, and additionally
  every visible but unfocused window at `Critical`; the focused window is
  never released, because there would be nothing to show in its place, and
  neither is a window the embedder has not declared app-presented (nobody
  would answer its redraw). Only the pixels go: the window keeps its client
  size, origin, furniture, cursor, viewport, and size state, still
  hit-tests, focuses, and resizes, and composites transparent until its app
  presents again. The buffer is user data, so it is overwritten before it is
  dropped. Because a present carries only a damage rectangle, every release
  queues a redraw request the embedder drains through `pending_redraws` —
  the crate asks for the repaint but never speaks the window protocol
  itself.

- The desktop layer (`compositor`): an optional `Surface` the session
  installs with `set_desktop`, composited between the opaque background
  fill and every window and taken back down with `clear_desktop`
  (`desktop_bounds` reports the footprint either answers). It carries no
  `WindowId`, which is exactly why it can never be raised, focused, moved,
  or restacked through the ordinary window z-order — nothing in that
  z-order can end up beneath it by accident, either. In
  `present_accelerated` it is encoded as its own hardware layer, directly
  on top of the background layer and beneath every window's, so the
  accelerated and software paths agree pixel-for-pixel. Installing,
  clearing, or replacing it damages exactly its old and new footprints,
  precisely as a window's own surface replacement does.
- Input that resolves to no window is reported, not swallowed: a primary
  press or a key with focus on the desktop comes back as `DesktopPressed`
  / `DesktopKey { key, modifiers, pressed }`, and pointer motion that
  lands on no window and starts no grab comes back as
  `DesktopPointerMoved` — carrying no position of its own, because the
  motion has already updated the router's own `pointer()`, which is where
  the desktop layer's owner reads it from. The router names where the
  input landed and takes no action of its own for any of the four.

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
marking nothing), the desktop layer (drawing over the background and under
every window, a smaller-than-screen layer leaving the background showing,
and installing/replacing/clearing each damaging exactly its footprint),
the backdrop blur (the box blur's own identities — a uniform field left
unchanged, an impulse spread symmetrically, radius 0 and a one-pixel
region identities — and the composited effect: a spread backdrop, a no-op
at radius 0, confinement to the window rectangle, the logical radius
following the output scale, rounded corners left alone, the accelerated
path falling back to software, and a change behind a frosted window
repainting it to exactly the pixels a whole-screen composite gives),
and input routing (hit-testing, click-to-activate focus
and raise, desktop-clears-focus, `DesktopPointerMoved` carrying no position
of its own and `DesktopKey` naming focus-on-desktop, programmatic
`focus`/`unfocus` with the
fail-closed unknown-window path, move-grab drag, and the fail-closed grab
edge cases), the cursor overlay (compositing above windows, move/hide
repaint with damage, a multi-sample sweep composing the byte-identical
frame a single move to the same place does), and cursor selection (the
move-grab/window-hint/desktop policy, controller shape switching,
re-rendering on scale and cursor-set changes, and reuse of a cached kind
when it recurs).

The window-furniture tests pin both halves of the reclaim contract: the
exact composited pixels of every band and the rounded rim corners — the
resize zone is invisible, so the corner carries client content, not a grip;
retained bytes that scale with the frame band and not the
window area, and never past the one-screenful ceiling however many windows
are open; a scale or theme change dropping every entry at once (including a
theme variant that keeps its `ThemeId`) against a title, focus, or resize
change dropping only that one window's; mild memory pressure emptying the
cache and refusing further growth; teardown overwriting every retained
strip; a minimised window's furniture being evicted ahead of a visible
window's; and the composited frame coming out byte-identical with the cache
warm, emptied, and unable to retain anything.

The releasable-content tests pin the release policy the same way: the pixel
bytes actually overwritten before the buffer is dropped and the retained
bytes falling to zero; a released window compositing transparent while the
rest of the desktop stays pixel-identical; a released window still
hit-testing, still drawing its furniture, still focusing and still resizing;
a full-window present after a release restoring pixel-identical content;
the whole band ladder (nothing at normal, hidden at mild, visible-unfocused
only at critical, the focused window never released at any band, and a
session-painted window never released at all); exactly one redraw request
queued per release; and an app that ignores the request leaving its window
blank while the desktop composites on unharmed.
