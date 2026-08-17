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
- Backdrop blur: a window can ask for the already-composited
  content behind its rectangle to be frosted before its own translucent
  pixels blend over it. Composition is back-to-front, so the back buffer
  already holds that backdrop: the compositor composes the layers below
  the window, blurs the back buffer inside that window's rectangle only,
  and resumes composing from the window itself. The effect is
  `lib/raster`'s shared `Surface::frost_region` — the one frosted glass the
  desktop has, so the taskbar and every popup it opens, and the login screen
  frosting a selected account tile, all draw through the same code. It copies
  the rectangle, blurs the copy with a separable box blur carrying running
  sums (cost proportional to the rectangle's area whatever the radius, over
  premultiplied channels including alpha, with samples past an edge
  replicating it, so the effect
  can neither pull a neighbour's pixels in nor write outside the rectangle
  and a uniform backdrop comes out unchanged), and mixes the blurred copy
  back weighted by the window's own rounded-corner coverage, so a rounded
  window's frosting fades across exactly the arc its pixels do. The radius
  is a desktop length in logical pixels resolved through the output's
  `Scale`. The compositor owns one `tairix_raster::BlurScratch`, grown to
  the largest frosted rectangle the session has needed and reused, so a
  frosted window allocates nothing after its first frame; a mode change
  releases it rather than carrying the old screen's pixels. Because those
  pixels are a function of the whole backdrop beneath them, damage touching a
  visible frosted window whose frost must be **recomputed** promotes that
  window's full bounds into one recompose rectangle, iterated until nothing
  grows; and `present_accelerated` takes the software
  fallback outright while any visible window is frosted, because a
  hardware layer is composed from its own pixels and cannot sample what is
  already behind it.
- Retained backdrops (`frost`): a **translucent or backdrop-blurred** window
  is composed over the picture beneath its rectangle, so every frame that
  touches it otherwise recomposes that whole stack. That backdrop is a
  function of the layers
  beneath it, the window's whole rectangle, its physical radius and the
  window's shape — and of *nothing at or above its own layer*. So it is kept in
  a `ReclaimCache` (`frost_cache`, one screenful, `lib/reclaim`'s shared
  desktop policy) and a window's own repaint copies it back instead of
  composing that stack again. A blur of radius zero leaves the composed layers
  exactly as it found them, so an unblurred translucent window retains its
  backdrop through the very same path (`Window::reads_backdrop`) rather than a
  second cache: a `64×24` repaint inside a frosted terminal cost **17.4 ms** and now
  costs **26 µs**, and its damage stays the rectangle it marked rather than
  growing to the window. The last three inputs are recorded in the entry and
  consulted on every lookup, so a change fails closed to whatever it cannot
  have left intact: a different radius keeps nothing, while a window that has
  **moved, resized or changed shape keeps its core** — in screen coordinates
  the retained pixels are still exactly right wherever neither the blur's
  replication nor the shape's corners reach, so only the border is blurred
  again (`Surface::frost_region_around`). Dragging a frosted, translucent
  terminal costs **9.30 ns/px** a sample where it cost **15.30**, and dragging
  a translucent unblurred one **6.95** where it cost **19.89** — it was the
  slowest window on the desktop to drag and is now cheaper than a frosted one.
  A backdrop is snapshotted with `Surface::overwrite`, a row copy rather than a
  composite onto a blank surface. The rectangle
  recorded is the whole window's, because one pushed off a screen edge is
  frosted from the row and column the screen begins at while its shape is read
  from its own top-left. The layers beneath
  are answered by dropping the entry whenever damage is marked *below* the
  window — which is what `mark`, `mark_layer` and `mark_overlay` distinguish,
  and why a cursor sample, a fade step, the window's own content, and a window
  dragged across it from above all keep it. How much of a frost may be reused is
  asked of the cache **once per frame** and remembered, so a reuse is recorded
  as a hit and refreshes the entry's recency, and the plan and the composite
  can never read different answers. The layers a frost writes over are not
  composed at all — composing them first is work the copy throws away — so a
  frame composes below a frost only outside what it will write: nothing under
  one reused whole, and only the ring the border blur reads under one reused in
  part. The cache is read-only for the whole of a
  composite pass and written at the end of it (`ReclaimCache::retain`, which
  counts no second lookup), so admitting one frost cannot evict another the
  same pass had already decided to reuse. A frost is a blurred image of the
  user's desktop, so a released entry is wiped, not merely dropped.
- Damage tracking (`tairix_geometry::Region`): only changed pixels are
  recomposited, and the region's rectangles are pairwise disjoint, so no
  pixel is composited or presented twice and two far-apart updates stay two
  small rectangles. A backdrop-blurred window whose frost must be recomputed
  is promoted to its whole
  rectangle (and subtracted from the rest of the damage) because its pixels
  read the whole backdrop beneath it.
  `Compositor::composite` returns the `Region` it actually
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
- **Dithered blending.** A blend into the 8-bit back buffer holds only
  `256 - a` of the levels the picture beneath it had, so rounding every
  pixel alike steps a smooth wallpaper into visible horizontal bands under
  a translucent window or a frosted panel. Every blended pixel therefore
  rounds at its own share of `tairix_raster::DitherRow`, resolved once per
  row from the **screen** row and read at the screen column, and
  `WindowRow::sample` scales opacity and corner coverage at the same bias.
  The pattern is a pure function of the screen position, so a recomposited
  rectangle matches the frame it replaces exactly and two damage rectangles
  that meet cannot seam; its mean is plain nearest rounding, so nothing
  lightens or darkens and no pixel moves by more than one level. Opaque
  runs are copied, not blended, so the fast path pays nothing for it.
- **Translucency stays in software.** A hardware layer is blended by the
  engine in the scan-out's own 8 bits with a fixed rounding, which is
  exactly what bands a picture under a translucent field, and no layer
  stack can express a per-pixel dither. `present_accelerated` therefore
  takes its software fallback when any visible window is translucent as a
  whole, alongside the backdrop-blur and reveal cases below. A window's own
  anti-aliased corner is not this case: partial coverage on a few edge
  pixels has no gradient to band. Where the compositor *bakes* a window into
  a layer (`Window::sample_local`), it reads the dither at the pixel's
  screen position, so a baked layer holds exactly what the software
  composite would have written there.
- Screen reveal (`set_reveal`/`reveal`): the whole screen scaled toward
  black, `u8::MAX` normal and `0` black, which is what the desktop session
  fades in over on start-up. It is applied at the one step in
  `compose_span` where a composited pixel becomes a scan-out byte, so every
  present path dims each pixel exactly once; the back buffer deliberately
  keeps the true composed colour, because a frosted window samples the
  backdrop out of it and a blur-split rectangle re-reads it, and dimming
  there would dim those pixels twice. Only `r`/`g`/`b` are scaled, through
  the crate's own `div255`, so alpha and the premultiplied invariant are
  untouched and the fade goes to black rather than to transparent. A full
  reveal short-circuits, so an unfaded screen costs one comparison per
  encoded pixel and is byte-identical to one that never heard of the
  reveal. A change damages the whole screen, a repeat damages nothing, and
  a mode change carries the strength over. `present_accelerated` takes its
  software fallback while a reveal is in flight, because a hardware layer
  is scanned out as the driver was handed it and would show at full
  strength while the fade ran.
- Transients (`add_transient_window`): an app-owned popup
  (`tairix_abi::window_ipc::WindowRequest::CreatePopup`) is an ordinary
  undecorated window that *belongs to* the one that opened it. It is
  inserted directly above its owner and any transient already there, and
  every restack afterwards moves the **family** — owner immediately below
  its transients — whichever member is named, so `raise` on a terminal
  brings its menu and `lower` takes it along. Nothing can be raised between
  the two, and no caller re-asserts the arrangement per frame: a family
  already at the end it is being restacked to is left completely alone, with
  no restack, no damage and not one allocation. That last point is the whole
  reason the link lives here. Re-deriving the arrangement per frame *drops
  the owner's retained backdrop*, and the desktop used to do exactly that:
  hovering an open menu cost a frosted, translucent terminal a full-window
  blur, capture and present on every pointer sample — the whole window's
  pixels, tens of times a second, for a menu row's highlight. Raising an
  unrelated window now places it above *both*, which is what a desktop
  should do: an open menu no longer pins its owner over its neighbours.
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
  precisely as a window's own surface replacement does. `repaint_desktop`
  repaints it in place, painting and marking only the rectangles of the
  `Region` its owner asks for: it is the bottom layer, so marking all of it
  recomposites every window above and re-blurs every frosted backdrop over
  it — an icon taking the hover must cost that icon. A layer that is absent
  or sized for another screen is allocated fresh and painted whole.
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

## What a frame costs, and what it skips

`Compositor::frame_stats` reports the work the last frame actually did, in
exact counts rather than durations: pixels damaged, layer contributions
blended, pixels copied from an opaque run, pixels frosted, pixels encoded,
dirty rectangles recomposed, driver calls made, and the furniture cache's
hits and misses (taken as the delta of the cache's own accounting, so there
is no second tally). Counts are a function of the scene, so a test asserts
them exactly and stays green under any load; the embedder owns the clock and
pairs its own timings with them.

The number to read first is **damaged versus blended versus screen**: it
turns "the desktop feels slow" into "4.2 M pixels blended to change 3 200".

A row is served by copying rather than blending only when the segment's
front-most window offers an **opaque run** at that column. These are the
only conditions, and widening them is a correctness change, not a tuning
knob:

- the source pixel's own alpha is 255 (read from the pixels, never from a
  client's claim, so a translucent terminal can never hide what shows
  through it),
- the window's opacity is 255,
- the row carries no rounded-corner coverage,
- the pixel is a client pixel inside the drawable extent (furniture is
  always blended),
- no screen reveal is in flight on an encoding segment, and the cursor
  draws nothing on this row.

Because the decision is per run inside the row loop, it *is* the
compositor's occlusion culling: the windows below, the desktop layer and the
root fill are all skipped for exactly those columns. A window that covers
only part of a dirty rectangle, or is opaque only in places, still saves
what it can. Runs are sought only within a blur segment, so a frosted
window remains a barrier and nothing a frost reads is ever skipped.

The columns between two copyable runs are one **segment**, and a segment is
composed a *layer* at a time over its whole width — the base fill, the
desktop row, each window row back to front, then the cursor — rather than a
column at a time through the whole stack. Each layer is a straight run of
source pixels at a screen column and a constant opacity, laid through
`lib/raster`'s one span composite (`blend_span`), so the arithmetic per pixel
is the same *over* at the same rounding while the layer decision, the
coordinate conversion and the bounds checks around it are paid once per run.
A pixel still sees its layers in the order it always did and the frames are
byte-identical; what changed is that measurement showed the dispatch, not the
blending, was the larger half of a translucent composite. Full-screen opaque
composition fell from **2.98 ns/px to 0.61**, and the translucent case from
**10.04 to 5.99**. The rows where coverage genuinely varies per column — a
rounded corner's arc, and the cursor — keep the column-by-column walk inside
their own contribution.

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
correctness (opaque and transparent edge cases), the screen reveal (a full
reveal's frame byte-identical to one the reveal never touched, a half
reveal scaling every composed pixel by the crate's own rounding while the
back buffer keeps the true colour, a zero reveal presenting black, the
premultiplied invariant at every strength, whole-screen damage on a change
and none on a repeat, the layer path declining mid-fade and resuming after
it, and a mode change keeping a fade in flight), per-region alpha
blending, rounded-corner masking, z-order, window move/hide/remove with
damage repaint, channel-order encoding, the present seam (no damage
presents nothing, disjoint rectangles present individually, whole-screen
damage presents once, and more rectangles than the limit collapse to one
bounding-box present), no-op updates marking nothing, edit-reported
content damage (offset by a decoration band, clipped to the client, empty
marking nothing), the desktop layer (drawing over the background and under
every window, a smaller-than-screen layer leaving the background showing,
and installing/replacing/clearing each damaging exactly its footprint),
the backdrop blur — the composited effect, the frost's own identities
being pinned in `lib/raster`: a spread backdrop, a no-op at radius 0 and
for an unknown or hidden window, confinement to the window rectangle, the
logical radius following the output scale, rounded corners left alone, the
accelerated path falling back to software, and a change behind a frosted
window repainting it to exactly the pixels a whole-screen composite gives —
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
