# Compositing window manager

`userland/gui/wm` (`tairix-wm`) is the user-space compositor for the
TAIRiX desktop (`AGENTS.md` §10). All compositing happens in user space;
the kernel only ships framebuffer access through a capability, and no
non-GUI crate depends on it (`AGENTS.md` §17.3). This page documents the
**compositor core** and the **input router** delivered in the first
Stage 7 increments, plus the optional **GPU-accelerated present path**;
theming and the taskbar build on them in later increments.

## Pipeline

The compositor turns a stack of windows into one scan-out frame:

1. Each window owns its content `Surface`: a dense, row-major buffer of
   **premultiplied-alpha** `Pixel`s. The buffer is *releasable* under
   memory pressure (see [Releasable window content](#releasable-window-content));
   a window whose pixels have gone keeps every other property and
   composites transparent until its app presents again.
2. `Compositor::composite` walks the damaged screen regions and, for
   every dirty pixel, blends each covering window *over* the opaque
   background, bottom-to-top in z-order, using the Porter–Duff *over*
   operator (`Pixel::over`). It returns the `Region` it actually
   recomposited (screen-clipped), which is what the present step moves.
3. Each composited pixel is scaled by the screen reveal (see
   [Screen reveal](#screen-reveal)) and encoded into a byte frame laid out
   for the active `DisplayMode` (`Rgba8888` or `Bgra8888`).
4. `Compositor::present` hands the changed part of that frame to a
   `Display` driver.

A row, not a pixel, is the unit of work: for each dirty row the
compositor resolves every covering layer's source row (`Window::row`),
the back-buffer row, and the frame row once, so a column is a slice index
and a blend rather than a coordinate conversion, a layer decision, and a
`y * stride + x * 4` offset recomputed per pixel. Only windows whose
bounds overlap the dirty rectangle are considered at all.

Because the root background is forced opaque, the final screen is always
fully opaque and its premultiplied channels equal their straight-alpha
form on scan-out.

The background is set at construction and changeable at runtime:
`Compositor::set_background` (a runtime theme switch is one call here)
forces the new colour opaque exactly as `new` does, marks the whole screen
dirty so the next composite repaints every pixel over it — windows and the
cursor re-blend on top unchanged — and returns `false` without damaging
anything when the colour is already in effect, so a caller can skip a
redundant present.

## Screen reveal

`Compositor::set_reveal(strength)` scales the whole screen toward black:
`u8::MAX` is the normal picture, `0` is black. It is what the desktop
session fades in over on start-up (see [the session](session.md)), and
nothing else in the compositor reads it.

It is applied at exactly one point — the step in `compose_span` where a
composited pixel becomes a scan-out byte — so every present path
(`composite`, `present`, `present_region`, the accelerated path's software
fallback) dims each pixel exactly once. The back buffer deliberately keeps
the **true** composed colour: a frosted window samples the backdrop out of
it, and a blur-split rectangle re-reads it for its second segment, so
dimming the buffer instead would dim those pixels twice.

The scaling reuses `div255`, multiplies only `r`/`g`/`b` and leaves alpha
untouched, so the premultiplied invariant (`channel <= a`) holds at every
strength and the screen fades to black rather than to transparent. A full
reveal short-circuits to the pixel itself, so a screen nobody is fading
costs one comparison per encoded pixel and is byte-identical to one that
never heard of the reveal. Changing the strength damages the whole screen,
because every presented pixel's value changed; setting the strength
already in force damages nothing. A mode change carries the strength over,
so a session that re-modes mid-fade keeps fading.

## The desktop layer

`Compositor::set_desktop` installs an optional `Surface` — the session's own
wallpaper-and-icons layer — anchored at the screen origin and composited
directly over the opaque background fill but beneath every window
(`clear_desktop` takes it down again; `desktop_bounds` reports the footprint
either answers, or `None` when none is installed). It deliberately carries
no `WindowId`: that is exactly why it can never be raised, focused, moved,
or restacked through the ordinary window z-order, and why nothing in that
z-order can ever end up beneath it by accident either. A surface smaller
than the screen simply leaves the root fill showing where it does not
reach, and one larger is clipped — the layer is never a reason to fail a
frame. Installing, replacing, or clearing it damages exactly its old and
new footprints, precisely as any other surface replacement does.

Pointer and keyboard input that resolves to no window is reported to the
desktop layer's owner rather than swallowed: see *Input routing*, below.

## Hardware acceleration

When the display driver exposes the optional
`AcceleratedDisplay` seam (`AGENTS.md` §10),
`Compositor::present_accelerated` lets the hardware composite the scene
instead of the CPU. It encodes the scene back-to-front as one solid
background layer, the desktop layer (when one is installed) directly on
top of it, one `AccelLayer` per visible window (its surface baked
with that window's opacity and rounded-corner coverage through the same
`sample_local` path the software compositor uses, so the hardware result
matches pixel-for-pixel), and the cursor on top, then hands the stack to
`AcceleratedDisplay::present_layers`.

The software path is always the fallback: if the scene exceeds the
engine's reported `AccelCaps` — more layers than it has planes, or a
layer larger than it can source — the compositor composites the whole
frame in software and presents it instead, so a hardware frame is never
partial (`AGENTS.md` §2.9). A **backdrop blur** takes that same fallback
unconditionally (`Compositor::has_backdrop_blur`): a hardware layer is
composed from its own pixels alone and cannot sample what is already
behind it, so a scene with any visible frosted window has no layer
encoding at all and goes through software rather than presenting a frame
the frosting is missing from. An **in-flight screen reveal** takes it too:
the engine scans a layer out as the driver was handed it, so nothing it
composes passes through the dimming and the screen would show at full
strength while the fade ran. `encode_layers` therefore declines while
`reveal() < u8::MAX` and resumes the moment the fade completes. The first
driver to implement the seam is the
Raspberry Pi VideoCore HVS plane compositor (see
[Display drivers](../drivers/display.md)).

## Premultiplied alpha

Working in premultiplied alpha makes the *over* operator a single
multiply-add per channel and keeps per-surface, per-region, and
rounded-corner coverage correct:

- `Color::premultiply` converts an authored straight-alpha colour into a
  stored `Pixel`.
- `Pixel::scale_alpha` applies an opacity factor (per-window opacity ×
  corner coverage) by scaling every channel — colour and alpha — at once.
- `Pixel::over` composites a premultiplied source over a premultiplied
  destination as `src + dst * (1 - src.a)`.

`Color`, `Pixel`, and `Surface` (and the `From<Rgba> for Color` theme edge)
are defined once in the shared rasteriser `lib/raster` and re-exported by the
window manager. The taskbar paints into the same `Surface` type and reuses the
same colour algebra without depending on the window manager (`AGENTS.md`
§17.4) and without a second implementation (`AGENTS.md` §2.2).

## Rounded corners

`Corners::Rounded { radius }` rounds a window's corners; `Corners::Square`
is the opt-out. Coverage in `0..=255` is computed by deterministic
supersampling on a fixed grid (no `sqrt`, which `core` lacks), so a pixel
on a corner arc receives anti-aliased partial coverage and the
anti-aliasing is exactly reproducible in tests. The radius is clamped to
half the shorter side. The taskbar's rounded edges reuse this same path
rather than a second implementation (`AGENTS.md` §2.2).

## Backdrop blur

A window may ask for the already-composited content *behind* its
rectangle to be blurred before its own — typically translucent — pixels
are blended over it, so a terminal or panel reads like frosted glass.
`Compositor::set_backdrop_blur(id, radius_px)` sets the radius in
**logical** pixels; `0` disables the effect. An app asks for its own
window's radius over the window channel
(`WindowClient::set_backdrop_blur`, below), which the protocol bounds at
`WINDOW_BACKDROP_BLUR_MAX_PX` — a validation limit on the compositor's
per-frame work, not a capacity that grows.

The radius is a desktop length, so it is authored once and converted to
physical pixels through the output's own `Scale::scale_length`, exactly as
every other desktop length is (see [Variable DPI](dpi.md)). The frosting
looks the same at every display density.

Composition is back-to-front, which is what makes the effect cheap: by the
time the compositor reaches a blurred window, the back buffer already
holds everything behind it. It therefore composes the layers *below* the
window, blurs the back buffer inside the window's rectangle, and resumes
composing from the blurred window itself over that frosted backdrop. Only
the final segment encodes the scan-out frame, so the intermediate stages
cost no wasted encoding, and several blurred windows in one stack simply
segment it further.

The effect itself is `lib/raster`'s shared `Surface::frost_region` — the
one frosted glass the desktop has, which the login screen frosting a
selected account tile draws through too. It copies the rectangle out of the
back buffer, blurs the copy, and mixes the blurred pixels back over the
originals at a per-pixel weight the caller supplies.

The blur is a **separable box blur**: a horizontal pass then a vertical
one, each carrying a running sum so the window slides by one add and one
subtract per output. The cost is proportional to the rectangle's *area*
whatever the radius, never to area × radius. The copy and the pass-to-pass
intermediate live in one `tairix_raster::BlurScratch` the compositor owns,
grown to the largest frosted rectangle the session has needed and reused,
so a frosted window allocates nothing after its first frame; a mode change
releases it rather than pinning the old screen's worth of pixels.

Every channel is averaged, alpha included: on premultiplied data that is
the same convex combination of the contributing colours that compositing
them would give, so the `colour <= alpha` invariant survives and no halo
appears at a translucent edge. Samples past an edge replicate that edge,
which confines the effect to the window's own rectangle — it can neither
pull a neighbour's pixels in nor write outside its bounds — and keeps the
divisor constant, so a uniform backdrop comes out exactly unchanged.

The mix weight is the window's own rounded-corner coverage (the single
`Window::shape` definition its *pixels* are weighted by), so a rounded
window's frosting fades out across exactly the arc its own pixels fade out
across and no square edge shows outside it. That coverage is asked for at
coordinates relative to the *rectangle's* own top-left, so a window
starting off screen is frosted from the row and column the screen begins
at while still reading its shape from its own corner.

## Input routing

`InputRouter` is the desktop's input-policy layer over the compositor's
scene graph. It tracks the pointer position and the focused window and
turns device-level pointer events (`InputEvent`) into window-manager
actions, reporting each through `InputResponse`:

- **Hit-testing** — `Compositor::window_at` returns the top-most visible
  window whose bounds contain a point, walking the z-order from the top
  down. Rounded corners are cosmetic and do not carve holes out of a
  window's input region (`AGENTS.md` §2.2).
- **Click-to-activate** — a primary-button press over a window raises it
  to the top of the z-order and gives it focus, returning
  `Activated { window, local }` with the press position in the window's
  surface coordinates. A press on the desktop background clears focus
  (`DesktopPressed`).
- **Input that lands on nothing is reported, not swallowed** — the desktop
  layer beneath the window stack is a real surface with a real owner, so
  pointer motion that resolves to no window and starts no grab comes back
  as `DesktopPointerMoved`, and a key event while focus rests on the
  desktop (rather than a window) comes back as
  `DesktopKey { key, modifiers, pressed }`. `DesktopPointerMoved` carries
  **no position of its own**: the motion has already updated the router's
  own `pointer()`, which is where the desktop layer's owner reads the
  position from, so the response need not duplicate it. The router takes
  no action of its own for either — it only names where the input landed,
  leaving the desktop layer's owner to interpret it (hover feedback,
  moving a selection, a drag it started itself).
- **Move-grabs** — dragging a window is an explicit grab started by
  `InputRouter::begin_move` (which decorations call when a press lands on
  a window's move handle, e.g. a title bar), not a behaviour armed on
  every press. Holding the grab offset constant, subsequent pointer
  motion drags the window (`Moved`); releasing the primary button, or the
  window vanishing mid-drag, ends it (`MoveEnded`). Separating content
  clicks from window dragging avoids a "drag anywhere" hack
  (`AGENTS.md` §2.1).

The router models *which* window owns the keyboard (`focused`); the key
encoding itself is a separate ABI concern and is not invented in the
compositor (`AGENTS.md` §2.4). The router never panics and fails closed:
`begin_move` without a focused (or still-known) window starts no grab
(`AGENTS.md` §2.9).

## Damage tracking

`tairix_geometry::Region` records the screen rectangles that changed since
the last frame (a window was added, moved, restyled, hidden, raised, or
removed). `Compositor::composite` recomputes only those pixels, returns the
region it recomposited, and clears the damage, so an idle desktop costs
nothing to recomposite.

The region's rectangles are pairwise **disjoint**: the same pixel is never
listed twice, however many updates marked it, so it is composited once and
presented once. Two far-apart updates stay two small rectangles rather than
collapsing into the bounding box between them. The type is shared
(`lib/geometry`) because controls report their own repaint rectangles with
it too.

A window with a backdrop blur is the one exception to "recompose exactly
what changed": its pixels are a function of the *whole* backdrop under its
rectangle, so a strip-sized repaint would spread a clipped neighbourhood and
seam against the pixels beside it. Damage touching such a window — when its
frost has to be recomputed — therefore promotes the whole of it into one
rectangle, and that rectangle is
subtracted from the rest of the damage — so the frosted window is recomposed
whole while unrelated damage elsewhere stays exactly as tight as it was
marked. Two frosted windows that overlap merge into a single rectangle,
because each reads what the other wrote.

When the frost does **not** have to be recomputed there is no neighbourhood to
spread and no promotion at all: the retained frost is copied back and the
damage stays the rectangle it was marked as. See [Retained frosted
backdrops](#retained-frosted-backdrops).

**An update that changes nothing marks nothing.** `move_window` to the
origin a window already has, and `set_corners` / `set_visible` /
`set_opacity` / `set_backdrop_blur` to the value already in effect,
repaint no pixel; they still return `true`, because `false` means only
"unknown window". This matters because a presenter re-issues exactly
those calls every frame, and each spurious mark would recomposite a whole
window for nothing. A replaced *surface* (`set_surface`) is always
assumed changed: comparing two whole buffers costs more than
recompositing the window.

**A blurred window is always repainted whole.** A frosted window's pixels
are a function of the *entire* backdrop under its rectangle, so
recomposing a strip of it would spread a neighbourhood clipped to that
strip and leave a seam. Every damage rectangle that touches a visible
blurred window's on-screen bounds therefore grows to cover all of them,
repeated until nothing grows — widening one window can bring the damage
into contact with a second. A change *behind* a frosted window (a window
moving past it, one presented pixel of the app underneath) consequently
refrosts the whole window, and an incremental repaint produces exactly the
pixels a whole-screen composite would. The sweep matches and adds only
screen-clipped rectangles, so damage that lies wholly off screen still
composites nothing and `has_damage` keeps its promise.

**A present reports its own damage.** `present_window_content` takes the
presented frame's extent and a conversion returning `(value, Rect)`, where
the rectangle is in content-local pixels; the compositor translates it by
the window's content origin and intersects it with the client rectangle, so
an empty rectangle marks nothing and an over-large one is clipped rather
than ever reaching a neighbouring window. The conversion reports the
rectangle rather than the caller declaring one up front because only the
conversion — having compared each pixel it wrote against the one already
there — knows what truly changed; a conservative rectangle handed down
beforehand would repaint pixels that never moved.

**The frame is the window manager's; the pixels are the client's.** A
window's content buffer is sized by the frame the *client* presents, never
by the window manager's own resize: `resize_window` and
`resize_window_client` move the geometry the compositor draws and lays the
furniture out from (`Window::client_size`) and do not touch the buffer, and
`present_window_content` establishes the buffer whenever the one held does
not describe the presented frame. This is what makes a live resize correct.
A resize-grab moves the frame on every pointer motion while the app is told
its new size once, when the drag settles, so in between the app is still
presenting the geometry it last knew. Reshaping its buffer under it would
refuse each of those presents, which an app cannot tell from a dead session;
instead the compositor simply draws the part of the buffer that lands inside
the client area (`Window::row`), which is exactly what the user should see —
and the drag costs no per-motion copy of the window's pixels. A buffer
established afresh carried nothing over, so the whole client area is marked
dirty rather than the rectangle the conversion reported.

**Cursor damage is the rectangle it left plus the one it reached.** The
pointer overlay is not damaged as it moves; `composite` diffs the
cursor's current footprint against the one the *previous* composite drew.
A desktop that pumps a whole batch of pointer samples before repainting
therefore recomposites two rectangles, not one per sample: no
intermediate position was ever drawn, so those pixels are already
correct, and the composited frame is byte-for-byte the one a single move
to the same place produces. A move that lands on the identical rectangle
repaints nothing. Replacement artwork (`set_cursor`) always repaints,
even on an identical rectangle, because the pointer picking up a text or
resize shape without moving changes the pixels there.

`Compositor::has_damage` answers exactly what the next composite would
produce: `true` if and only if at least one pixel would be recomposited,
counting a pending cursor move or artwork change and discounting damage
marked wholly off screen. A session driving a wake loop can therefore
skip a frame outright when it is `false` without ever missing a repaint.

## Presenting only what changed

`Compositor::present` composites and then moves the smallest sensible
number of bytes:

- **No damage presents nothing.** The display driver is not called at
  all, so a wake that changed nothing costs neither the whole-frame
  shared-memory copy nor the driver blit. The first frame still shows: a
  new compositor marks the whole screen dirty.
- **Whole-screen damage is one `Display::present`.**
- **Anything else is one `Display::present_region` per disjoint dirty
  rectangle**, so the bytes moved are proportional to what changed rather
  than to the bounding box of scattered damage — a dirty taskbar strip
  along the bottom edge plus a cursor near the top is two small blits,
  not a near-full-screen one.
- **Past `MAX_PRESENT_REGIONS` rectangles, one bounding-box present
  replaces the batch.** Each region present is a synchronous round trip to
  the display service whose fixed cost is paid however few pixels it
  carries, while a larger copy costs only more bytes; beyond a handful of
  rectangles the round trips cost more than one call copying their box.

## Retained frosted backdrops

A frost is a function of exactly four things: the pixels the layers beneath it
composed, the window's own rectangle, its physical blur radius, and the window
shape the blurred copy is mixed back through. It is **not** a function of
anything at or above its own layer — the blur happens before the window's own
translucent pixels are blended over it, and everything stacked above is blended
afterwards. The pointer moving inside a frosted terminal and a window dragged
across one, the two dominant interactions on the desktop, therefore change
nothing the frost reads, yet either used to re-blur the whole window per sample:
two separable passes over every pixel of it, measured at 17.4 ms for a 64×24
repaint against 0.9 µs for the same repaint over an opaque stack.

The compositor keeps each frosted window's backdrop instead, in a bounded,
pressure-governed cache on the same terms as the window furniture above:
ceilinged at one screenful of pixels (no more of it can be visible at once),
released when the memory-pressure band tightens, and **wiped** on release,
because a frost is a blurred image of whatever the user had on screen. The same
repaint now costs 26 µs.

How a retained frost is known to be still right:

- **The rectangle, the radius, and the shape are recorded in the entry** and
  compared on every lookup. A moved, resized, re-rounded, re-radiused, or
  rescaled window fails that comparison and is blurred again, so the check
  holds even if the compositor forgot to say anything had changed. The
  rectangle recorded is the window's whole one, not the part of it on screen: a
  window pushed off an edge is frosted from the row and column the screen
  begins at while its shape is still read from its own top-left, so two
  positions that clip to the same on-screen rectangle are two different frosts.
- **The pixels beneath it cannot be self-checked** without reading the ones the
  copy was meant to save, so the entry is dropped when the compositor marks
  damage that could have changed them. Marking distinguishes three cases: a
  change confined to *one window's own layer* — its content, position, size,
  shape or furniture — drops only the frosts of windows stacked above that one,
  which is why a window dragged across a frosted one costs it nothing; a change
  that is not confined to a layer (the root fill, the desktop layer, the
  density or theme, or a restacking, which changes *which* layers a frost sees)
  drops every frost it reaches; and a change no frost can read at all — the
  cursor overlay, composed after every window, and the screen reveal, applied
  only as a pixel is encoded for scan-out — drops none.
- **Whether a frost may be reused is asked once per frame and remembered.** The
  recompose plan and the composite that follows it both need the answer, and
  two lookups could disagree — which would leave a window the plan did not
  widen for being blurred over a rectangle whose lower layers the frame never
  composed. That one lookup is also what the cache counts, so a reuse reads as
  a hit and refreshes the entry's recency: the frost every frame serves must
  not be the first one a pressured cache gives back.
- **Recomputing one frost drops any frost above it that overlaps**, because a
  blur spreads the change far past the rectangle that caused it, so the window
  above reads different bytes even where the damage never reached.
- **A density or mode change empties the cache** through its epoch. Both are
  already caught per entry, so this is not what keeps a stale frost off the
  screen — it is what stops a superseded one *staying charged* against the
  budget until it is next looked up. A window that stops frosting altogether is
  never looked up again, so setting its blur radius to zero releases its entry
  outright rather than leaving a screenful of dead pixels charged.

The cache is read-only for the whole of a composite pass and written at the end
of it: admitting a frost mid-pass could evict one the same pass had already
decided to reuse, and that reuse would then blur a rectangle whose lower layers
the frame only composed where the damage happened to fall.

Losing a frost costs blur work and never a wrong pixel, which is what makes the
cache an accelerator rather than a correctness requirement. That is asserted,
not asserted-about: the compositor's tests compose one scene twice — once
reusing retained frosts, once blurring afresh every frame — through some thirty
mutations (content presents above, below and inside the frost, cursor motion, a
fade, restacking, geometry, radius, corner, scale, theme and mode changes,
overlapping frosts, a frost clipped by the screen edge, and window removal) and
require the scan-out frame *and* the back buffer to be byte-identical after
every one.

## What one frame cost (`FrameStats`)

"The desktop feels slow" is not a defect report. `Compositor::frame_stats`
returns the counts the composite pass that just ran actually paid, so the
complaint becomes a measurement: *we blended 4.2 M pixels to change 3 200.*
The accumulator is reset at the start of every composite, so a snapshot
describes that frame and nothing else.

Every field is a **count of work, never a duration**. Counts are exactly
reproducible for a given scene, so a test may assert them and stay green under
any machine load, which a wall-clock figure cannot. The compositor is `no_std`
and holds no clock: an embedder that wants a duration owns the clock and pairs
its own measurement with these counts. Counters saturate rather than wrap — a
wrapped one would read as a suspiciously small frame.

| Count | What it says |
|---|---|
| `damaged_px` | Screen pixels inside the frame's dirty rectangles, after screen clipping and after any blurred window whose frost had to be recomputed widened the damage it touched. The size of the job — the denominator for everything else. |
| `blended_px` | Layer contributions blended through *over*. Contributions, not screen positions: a pixel two windows both draw at is one damaged pixel and two blends, so this may legitimately exceed the damage — and that ratio is what says whether the frame paid for depth nobody can see. |
| `opaque_px` | Pixels resolved by copying a fully opaque run of the front window's own pixels. Each cost no blend, and everything beneath it was skipped, which is why `blended_px` falls as this rises. |
| `blur_px` | Pixels rewritten by a *recomputed* backdrop frost. A frost served from the retained one is copied rather than blurred and counts nothing here, so this is exactly the blur work the frame could not avoid — and zero is what a repaint inside a frosted window should read. |
| `encoded_px` | Composed pixels converted to scan-out bytes. |
| `dirty_rects` | Dirty rectangles the frame recomposed. |
| `present_calls` | Calls the frame made into the display driver to publish itself — the round trips of the section above. |
| `chrome_hits` / `chrome_misses` | Window-furniture lookups served from the retained cache versus rendered again, whether the cache then kept them or refused. |

The reading that matters is **damaged vs blended vs screen pixels**: damage
far below the screen means the damage tracking is working, and blends far
above the damage means the frame is compositing depth the user cannot see.
`FrameStats::is_idle()` distinguishes a frame that recomposed nothing from one
that recomposed a little, so a wake that did no work reports *idle* rather
than a row of zeros pretending to be a frame.

## Server-side window decorations

Window decorations — a title bar carrying the four command controls in two
corner clusters (put-to-back and close at the leading edge, minimize and
size-toggle at the trailing one) with the identity and title centred between
them, and the frame rim — are drawn by the **window manager**, never by an app
(`AGENTS.md` §10, `plans/GUI-CONTROLS-DESIGN.md` §1, §11.17–§11.23;
`plans/COMPOSITOR-WORK.md`). An app supplies only its content surface and
typed window metadata; it can neither paint over nor receive input from
the chrome. The furniture family itself lives once in
`lib/controls::window` (`WindowFrame`, `TitleBar`, `WindowControl`,
`ResizeGrabber`) and is composed here, so there is no second visual recipe
(`AGENTS.md` §2.2).

- **Reserved band (geometry).** `Compositor::set_window_frame` attaches a
  `WindowFrame` and reserves a furniture band *around* the client from the
  frame's `FrameInsets` at the active `Scale` and `Theme`: the window's
  outer `bounds` grow to hold the decoration and the content surface is
  presented inset at `window_client_rect`, so the client never overlaps the
  furniture. That band is the border plus the title bar above and the **thin
  frame rim** on the other three edges — the same for a resizable window as
  for a fixed one, because a band wide enough to grab would show as dead
  space around every app's content. A resizable window's extra grab room
  lives in the hit map instead (below), never in this geometry.
  `clear_window_frame` collapses the band back to the bare surface. A DPI
  (`set_scale`) or theme (`set_theme`) change re-resolves the band for every
  decorated window.
- **Rendering.** A decorated window's furniture is a `WindowChrome`: the
  four strips the frame actually draws into — the top (title) band, the
  bottom band, and the two side borders — painted through
  `WindowFrame::render` / `TitleBar::render` (rim, body, the owning
  application's identity icon, the sanitised title text via `lib/font`, and
  the four command controls), using the one
  `lib/raster` fill and the shared rounded-corner path — so the rim's
  rounded corners stay transparent and the desktop shows through. Only the
  strips are kept: the region between them is never sampled (the compositor
  draws the window's own content there), so retained bytes follow the band
  thickness and not the window area — a 1920×1080 window's furniture costs
  roughly a thirty-fifth of what one outer-sized surface did. The compositor
  samples those strips in the reserved band and the client content inside
  them (`Window::row` /
  `Window::sample_local`), for both the software and the
  hardware-accelerated present paths; a screen row needs at most two of
  them, since a row is either in the title/bottom band or crosses the client
  between the two side borders. The furniture is animation-free, so it is
  reduced-motion correct by construction, and high contrast thickens the
  command-glyph strokes.
- **Retention (bounded and reclaimable).** The strips are *derived* pixels,
  so the compositor holds them in one shared, pressure-governed
  `ReclaimCache` keyed by `WindowId` rather than each window pinning its own
  copy for as long as it exists. The cache is ceilinged at one screenful —
  no more furniture than fills the screen can be visible at once, so
  anything above that belongs to minimised, off-screen, or stacked-under
  windows, and those are exactly the entries eviction takes first. It is
  charged to the owning seat, and because furniture carries the window's
  title it is *wiped*, not merely dropped, on release. The embedder builds
  it (`tairix_wm::chrome_cache`) from the real output size, seat, pressure
  gauge, and audit sink and hands it to `Compositor::new`, exactly as it
  does the cursor and taskbar-icon caches; the session trims all three when
  the kernel reports a deeper memory-pressure band and tears them down at
  logout or seat loss. A single window's change (title, focus, resize,
  size-state, decorating, closing) releases just that window's entry; only a
  DPI or theme change moves the cache generation and drops every entry at
  once. The cache is an accelerator and never a correctness requirement:
  furniture the cache refuses — an exhausted budget, a pressure band that
  forbids growth — is rendered for that frame alone, and the composited
  output is byte-identical whether the cache is warm, has just been emptied,
  or can retain nothing at all.
- **One funnel decides the repaint and the retention.** Every mutation that
  can change how a frame is drawn runs through one internal helper that hands
  the mutation a damage sink, marks exactly the rectangles it reported over
  that window's layer, and releases the window's retained furniture *only when
  something was reported*. So a mutation that changes no drawn pixel — a title
  set to the label already there, an activation re-asserted, a pointer sample
  crossing the drag region — marks nothing and keeps its rendered strips, and
  a hover that reaches one command control costs that control's rectangle
  rather than the band it sits in. Neither the marking nor the invalidation is
  a caller's to remember.
- **Activation and title.** `Compositor::set_active_frame` repaints a
  window's title and controls for the focused/unfocused state the
  `InputRouter` tracks. The *rim* is not part of that: every window wears the
  one quiet `frame` neutral at every activation, because the rim is the line
  the eye reads a window's shape by — brightening it on focus made the
  boundary the loudest mark on the desktop and left every other window looking
  switched off. Focus is the title bar's to carry (its text sits at
  `on_surface` while active and `on_surface_muted` while not), joined under
  heavy contrast by a doubled inner rim line so the distinction is a
  difference in shape too. `Compositor::set_window_title` repaints the title
  bar with the (untrusted, sanitised) `WindowTitle` the channel already
  carries. An activation change repaints the four furniture bands (the rim
  contrast is a band-wide change under heavy contrast); a title edit repaints
  the title band alone. Neither recomposites the client.

- **Furniture hit map.** `Compositor::frame_hit` classifies a screen point
  against a decorated window's `WindowFrame::hit`, returning a typed
  `FurniturePart` (title bar, a command control, a resize edge, the inert
  rim, or the client). The `InputRouter` consults it *before* the client and
  before the root-viewport scrollbar hit map, so a press on the frame is never
  reported to the app as `Activated` and an app look-alike inside the client
  can never impersonate a real frame control (`plans/GUI-CONTROLS-DESIGN.md`
  §1, §11.17–§11.18). A non-resizable window classifies its border as inert
  `Frame`, never a resize edge, so a fixed-size window cannot be dragged
  larger and every pixel of its client reaches it. The client-press position
  the app receives is reported relative to the inset **client** rectangle, so
  decorating a window never shifts its content coordinates.
- **The resize zone is invisible (it overlaps the client).** Because the band
  is only the thin rim, a resizable window's resize edges reach *inward* over
  the client's outermost `hit_slop` pixels — the invisible resize border
  macOS, GNOME, and Windows use. A press there is `ResizeEdge`, not `Client`:
  the app still draws those pixels but does not receive presses on them, the
  accepted trade for a border that costs no visible space. Since the router
  consults the frame first, that outer strip also wins over a window's
  root-viewport scrollbar furniture. Drawing stays strictly separated even
  so — the frame paints no furniture mark inside the client.
- **Pointer and keyboard routing.** A title-bar press begins the cooperative
  move-grab; a command-control press captures the frame, feeds the click to
  `TitleBar::on_pointer`, and emits `WindowControl { window, control }` on the
  completed release; a resize-edge press (resizable windows only) drives the
  shared `ResizeGrabber`. When the frame furniture holds the keyboard, arrows
  move focus between the controls and Space/Enter activate one
  (`Compositor::frame_key` → `TitleBar::on_key`); a client press returns the
  keyboard to the app. The furniture controls report their own repainted
  rectangles into the sink the compositor hands them, so a hover, a press, and
  a keyboard focus move each cost only the control that changed — a focus move
  reports the control the ring left and the one it reached, never the strip
  between them. The router itself carries no damage: repainting belongs to the
  compositor, which owns the sink at the point the frame is mutated.
- **Typed lifecycle (no new syscall).** Each command control maps to a window
  lifecycle action in one shared place
  (`tairix_desktop_session::window_control_event`), so the live serve loop and
  the tests drive the same rule: **Close** returns
  `WindowEvent::CloseRequested` (the app tears down cooperatively — the window
  manager never destroys a window behind the app's back); a **secondary**
  press on Close is a distinct gesture, `WindowEvent::AlternateCloseRequested`
  (`window_control_alternate_event`), which closes nothing itself — the file
  manager reads it as "go up a folder", an app with no such notion ignores it,
  and a session-owned window with no channel drops it; **Minimize** hides
  the window, marks its taskbar entry minimised, drops focus, and returns
  `WindowEvent::Minimized`; **PutToBack** restacks to the bottom of the
  z-order with no app-ward event; **SizeToggle** maximizes to the session work
  area (screen minus the taskbar) or restores, returning `WindowEvent::Resized`
  with the new client size (nothing for a non-resizable window). These ride the
  existing window path, owner-validated by the engine; there is no ambient
  authority and no privileged force-quit button (`AGENTS.md` §4, §5.4).

## Releasable window content

A window's content surface is the compositor's single owned copy of the
app's pixels, and on a full-screen window it is the largest single
allocation the desktop holds. A minimised window that nobody can see
would otherwise pin megabytes indefinitely, so content is **releasable**:
under memory pressure the compositor gives the buffer back and asks the
owning app to present again.

This is deliberately **not** a keyed cache like the furniture, cursor,
and icon caches. Evicting a visible window's pixels is a *visual defect*,
not a slowdown, so eviction cannot be driven by recency — it has to be
driven by what the user can currently see. Content is therefore a
pressure-driven release **policy** over the same shared `PressureGauge`
and the same `tairix_reclaim::shrink_target` ordering the caches use: one
memory model, two mechanisms suited to two different kinds of memory.

- **What survives a release.** Only the pixels go. The window keeps its
  client size (`Window::client_size`, retained independently of the
  buffer), origin, z-order, visibility, furniture, cursor, viewport, and
  size state, so a released window still hit-tests, still draws its title
  bar and borders, still takes focus, and still resizes. It composites as
  fully transparent — the desktop shows through — and everything else on
  screen is unchanged.
- **Releasing wipes.** The buffer holds user data, so
  `Window::release_content` overwrites every pixel before dropping the
  allocation rather than trusting the allocator to have cleared it.
- **The redraw handshake.** A present carries only a *damage rectangle*,
  so a re-established surface starts transparent and is correct only once
  a full-window present arrives. Every release therefore queues a
  `WindowEvent::RedrawRequested` for that window, drained by the embedder
  through `Compositor::pending_redraws`. The compositor never reaches for
  the window protocol itself — the wm crate has no dependency on the
  window-server side — and it queues the same request when a window with
  no content is made visible again. `lib/window` answers the event on the
  app's behalf by re-presenting its last frame with full-window damage,
  so an app that does nothing still gets its pixels back; an app that
  ignores the event simply leaves its window blank while the desktop
  keeps running.
- **The pressure ladder** (`Compositor::release_content_under_pressure`,
  run by the session on a band change):
  - **Normal** — nothing is released. There is no reason to make an app
    repaint while memory is plentiful.
  - **Mild and deeper** — every **hidden or minimised** window's content
    goes. Nobody is looking at it, so the release is invisible and the
    win is the whole surface.
  - **Critical** — additionally every **visible but unfocused** window,
    each with an immediate redraw request. A background window blank for
    a frame is a far better outcome than exhausting memory.
  - The **focused** window is never released at any band: there would be
    nothing to show in its place.
- **Only windows an app presents.** A window the session paints itself —
  the taskbar, the lock screen, the file picker, a confirmation prompt —
  has no client to answer a redraw request, so releasing it would blank
  it permanently. `Compositor::set_app_presented` declares a window
  app-presented and the release policy skips every window that has not;
  the default is `false`, so a window is spared unless the embedder
  explicitly says an app owns its pixels.

## Decorated windows in the live session

The desktop session (`userland/gui/session`) turns decorations on for every
**served application window**. When a window opens over the channel,
`ShellWindowHost::window_opened` opens the bare window through the shell and
then calls `DesktopShell::decorate_window`, which attaches a `WindowFrame`
(always movable by its title bar) and labels its title bar with the channel's
`WindowTitle`.

The title bar also carries the **owning application's identity icon**, leading
the group that is centred in the span the two command clusters leave between
them, with the title text after it. The icon is inert: it drags the window like
the rest of the band and is never a control. A window with no identity reserves
no slot and its title is centred on its own.

- **Attested, never claimed.** `WindowServer` hands
  `WindowHost::window_opened` the caller's kernel-attested `ProcId`, which
  `ShellWindowHost` records against the compositor window it just opened.
  The session's `resolve_window_identities` then drains those records — the
  step immediately after the serve pass, because the attested-caller table
  and the launch records are both borrowed while a request is served — maps
  each pid to the bundle the desktop launched (`LaunchTable`) and resolves
  that bundle's own icon through the one chain a taskbar pin uses
  (`bundle_manifest_path` → `decode_bundle_manifest` → `bundle_icon_source`
  → the session's single `ArtworkCache` and sandboxed rasteriser). No string
  an app supplies can choose the icon, and one app's pid cannot yield another
  app's artwork.
- **Total resolution, and a window always opens.** An unidentified caller (a
  shell-spawned app, a child process) gets no icon at all rather than a
  fabricated badge. An identified bundle whose artwork is missing, refused,
  or undecodable gets `IconKind::AppBundle` with no artwork — the built-in
  glyph — so a slot is never blank. Every failure is a missing icon, never a
  failed window open.
- **Rasterised once, at the size drawn.** `Compositor::window_title_icon_side`
  reports the slot side for that window's laid-out title band at the active
  scale, and `Compositor::set_window_identity` takes the identity plus the
  artwork already rasterised at it, dirtying only the title band and dropping
  only that window's chrome-cache entry. A second window of the same
  application costs a cache lookup: the shared `ArtworkCache` serves it
  without re-reading or re-decoding the asset.

The title text elides with the shared `ELLIPSIS` mark rather than being cut,
because a title may be a path. A group wider than the span stops being centred
and pins to the span's leading edge, so the icon and the start of the title
stay put as the window narrows and only the tail is marked.

An app retitles its own window over the channel with
`WindowRequest::SetTitle`, which the server admits only from the window's
owner; `ShellWindowHost::window_retitled` moves the title bar and the taskbar
entry label from that one call, so the two cannot diverge.

Files, the terminal, and any future windowed app are decorated
this way with **no per-app decoration code** — the one place a served window is
dressed is the window manager.

Whether the frame is **resizable** is the opening app's own choice, carried on
its window `Create` (`WindowRequest::Create { resizable, .. }` → `WindowClient::create`'s
`resizable` argument → `WindowHost::window_opened`). A resizable window gets a
live maximize/restore size toggle and the invisible resize edges above; the app
re-lays-out to each new client size the window manager reports
(`WindowEvent::Resized`), re-mapping its frame region with `WindowRequest::Resize`
so the resize keeps the window identity. A fixed-size app passes `resizable:
false` — every client pixel reaches it and the size-toggle is inert — so an
app that renders at one size is never handed a size it did not ask to handle.
The file **viewer** (`userland/apps/viewer`) is a resizable app: it re-wraps
its text to the new width, preserves the reader's scroll position across the
resize, and fails closed (keeping the current surface) if a new frame region
cannot be allocated or the session refuses the re-map. Files and the terminal
open resizable too.

`DesktopShell::sync_active_frame` keeps exactly one window showing its active
frame: on every focus change — a click-to-activate press, a taskbar activation,
an open, a close, a minimize — the newly focused decorated window is activated
and the previously active one reverts to inactive. It is a no-op for an
undecorated focus, so the session's own **trusted file picker** — session
chrome dismissed by its own keys, not an app the window manager dresses — opens
undecorated and never gains an inert title bar, while still correctly
deactivating whatever app window it drew focus away from.

## App-owned popup surfaces

A context menu or a settings sheet drawn *inside* its app's own window is
clipped the moment the user shrinks that window. The fix is a **popup
surface**: an undecorated, app-positioned window stacked directly above its
parent, which any app can open for any transient overlay
(`plans/APPWIN.md` AW6).

- **One new request.** `WindowRequest::CreatePopup { parent_window_id,
  shm_handle, event_endpoint, frame_count, width_px, height_px,
  stride_bytes, format, offset_x, offset_y }`, reached from
  `WindowClient::create_popup` with one `tairix_window::PopupSpec`. It
  carries no title (a popup is not a taskbar entry) and no `resizable`
  flag (the app sizes it, the user does not drag it). A popup's semantics
  differ from a top-level window's, so it is its own variant rather than
  extra `Create` fields.
- **The offset is parent-relative, and the session clamps it.** An app is
  never told its own window's screen position, so it asks in physical
  pixels from its **parent's client origin**; the session resolves the
  parent's current origin, adds the offset, and clamps the whole popup onto
  the screen with the one shared `tairix_geometry::Rect::clamped_onto`
  rule the taskbar's menus already use (`AGENTS.md` §2.2). A negative or
  over-large offset is therefore a legitimate request — an overlay bigger
  than its owner's window (the terminal's settings sheet over a tiny
  window) still opens whole and on screen.
- **Owner-validated, and on the same budget.** The engine refuses a kernel
  caller, requires that `parent_window_id` is a live window the
  **kernel-attested** caller owns (a foreign or unknown parent answers
  `NotFound` — no existence oracle), and validates the geometry exactly as
  a `Create` does. A popup counts against the same
  `WINDOWS_PER_CLIENT_MAX` cap, so "popup" cannot be used to pin more
  shared memory than `Create` may (`AGENTS.md` §5.4).
- **Undecorated by construction, not by a flag.**
  `ShellWindowHost::popup_opened` opens it through
  `DesktopShell::open_popup_window`, which adds the window to the
  compositor and raises it but never calls `decorate_window` and never
  opens a taskbar entry — the same path the session's own trusted file
  picker already takes. No protocol "undecorated" bit exists to be
  forged.
- **Stacked directly above its parent.** The compositor's z-order is one
  flat stack, so the session re-asserts the coupling once per wake,
  immediately before `present`
  (`SessionWindows::keep_popups_stacked` → `raise(parent)` then
  `raise(popup)`), exactly as the lock overlay keeps itself topmost.
  Nothing raised earlier in that frame can land between a parent and its
  popup, and the compositor gains no popup concept of its own.
- **Parent death takes the popup with it.** Closing the parent — over the
  channel, from the frame's close control, or by the owning client dying
  (`client_exited`) — tears down every popup keyed to it; a `Close` naming
  the popup's own id tears down only the popup. The session drops the
  parent→popup link on either path, so no stale link can outlive a window.
- **Presenting is unchanged.** `Present`, `SetBackdropBlur`, and `Close`
  act on a popup's id exactly as on a top-level id, and the popup's own
  events (pointer, key) arrive under its own window id with
  **popup-local** coordinates, so an app hit-tests its overlay against the
  popup's own viewport. One event mailbox serves both windows; the app
  demultiplexes on `WindowEvent::window_id`.

The first consumer is the graphical terminal, whose context menu and
settings sheet are each a popup (`plans/GUI-TERMINAL.md` §9).

## Failing closed

Every fallible entry point returns a `Result`/`Option` rather than
panicking (`AGENTS.md` §2.9): `Compositor::new` and `Surface::new` return
`None` for a surface too large to allocate or a stride too small for one
scanline, and an unsupported pixel format is refused at construction
rather than guessed (`AGENTS.md` §2.1). There is no `unsafe` in the
crate.

## Graphical assets (SVG-first)

Every WM/desktop graphical asset — cursors, icons, notification glyphs,
window-chrome artwork, and theme decorations — is authored as **SVG**
(`AGENTS.md` §10). One scalable source keeps the asset crisp at any DPI or
UI `Scale`, the same reasoning that makes the cursors vector artwork
(see [Pointer cursors](./cursors.md)).

SVG is a *source* format, not a hot-path format. An asset is decoded and
rasterised/converted **once** at the active `Scale` into the fast-draw form
the compositor blits — a `lib/raster` `Surface`, or an intermediate vector
form such as `lib/cursor`'s — and that form is cached, re-rendered only when
the scale or theme changes, so compositing never touches an SVG parser and
the desktop stays quick. There is exactly one rasterisation/blend path
(`lib/raster`); the asset pipeline does not add a second (`AGENTS.md` §2.2).

SVG is untrusted input, so decoding runs through the curated §16.4
image-decoding shared library inside a minimum-capability parser sandbox
(`AGENTS.md` §19.5); a malformed or unrenderable asset fails closed to a
fallback rather than crashing the compositor (`AGENTS.md` §2.9).
Pre-rasterised bitmaps may exist as a cache or fallback, never as the only
path.

## The window channel (`tairix_abi::window_ipc` + `lib/window`)

Application windows reach the compositor over the **window channel**
(`plans/APPWIN.md` AW2): a fixed-width, versioned IPC protocol on the
reserved `WINDOW_ENDPOINT` (squat-protected — binding it requires
`CAP_IPC_BIND_PRIVILEGED`, exactly like the display service's endpoint).
The desktop session serves it; an app's `Run` binary calls it.

The transport is zero-copy, the display protocol's shape one layer up: an
app `shm_create`s a region holding its window frames, `shm_grant`s it to
the session once (`Create`), and thereafter presents by **frame index**
plus a damage rectangle (`Present`) — pixels never cross the IPC. A
`Create` also carries the frame geometry, a bounded, validated
`WindowTitle` (UTF-8, no control characters — it crosses into the taskbar
renderer), and the app's own **event endpoint**; the reply is the
session-minted, never-reused window id. Input travels the other way as
fixed-width `WindowEvent`s — focus changes, key events (embedding the one
desktop `KeyInput` codec), window-local pointer events, `CloseRequested`
(the app owns the close decision), and `RedrawRequested` (the session
released this window's retained content to reclaim memory and needs it
presented again) — delivered to that endpoint, where the app **parks**
until one arrives; it never polls.

An app also sets its own window's **backdrop-blur** radius over the
channel: `WindowClient::set_backdrop_blur(window_id, radius_px)` sends
`WindowRequest::SetBackdropBlur`, whose decode refuses a radius above
`WINDOW_BACKDROP_BLUR_MAX_PX` and a non-zero reserved tail. The engine
keys it to the attested window owner exactly as it does a present — a
radius set on another client's window answers `NotFound` — and hands the
validated pair to `WindowHost::backdrop_blur_set`, which the session
forwards to `Compositor::set_backdrop_blur` for that window and no other
(see [Backdrop blur](#backdrop-blur)).

`lib/window` hosts both halves of the behaviour so they cannot drift:

- `WindowServer` — the engine the session composes. Every request is
  attributed to the kernel-attested `ProcId` of the in-flight caller
  (`call_peer_origin` behind the `CallerIdentity` seam), and every window
  is keyed to its creator: a `Present` or `Close` naming another client's
  window answers `NotFound`, indistinguishable from a window that never
  existed. The granted region is mapped **once** at create (the shared
  `tairix_display::ShmMapper` seam) and validated to hold every frame;
  each present hands the session's compositor bridge (`WindowHost`) a
  bounds-checked frame slice and a damage rectangle validated inside the
  window's surface. A per-client window cap bounds pinned memory, a dead
  client's windows are torn down via `client_exited`, and app-ward events
  are validated against the live window before delivery.
- `WindowClient` / `WindowEvents` — the app half over the `WindowTransport`
  (`ipc_call`) and `EventSource` (parked endpoint wait) seams. The client
  remembers each window's last presented frame index and extent, so
  `WindowEvents::wait` answers a `RedrawRequested` by re-presenting that
  frame with full-window damage before returning the event to the app —
  no app has to implement the handshake, and an app that wants to render
  genuinely fresh pixels still sees the event.

Every decode fails closed (`tairix_abi::window_ipc`, enrolled in the
`fuzz_decode` harness), and the loopback suite in `lib/window/src/tests.rs`
proves both halves against one real server: ownership isolation,
create/present bounds, the client cap, refused-open rollback, teardown,
and event routing. The desktop session serves the endpoint live (`plans/APPWIN.md` AW3): it
binds `WINDOW_ENDPOINT` under the authority of its kernel-attested seat
lease, dispatches its wait-set loop on the woken member's token (a
`call_recv` with nothing pending blocks, so only a window-endpoint wake
recvs), bridges served windows into the shell (composited window +
taskbar task per client window), and routes input app-ward over the
event sink — a dead client's kernel-reclaimed port tears its windows
down. The first windowed app is the files browser
(`userland/apps/files`), spawned from the taskbar's permanent Files
button and proven end to end by the autoload QEMU vertical's
click-through (two verified screendumps: desktop, served window); the
terminal landed with AW4, spawned through the program-library popup's
catalog entry (`plans/NEW-TASKBAR.md` T5).

The channel also carries the desktop's **trusted file picker**
(`plans/APPWIN.md` AW5, `plans/CAPABILITY_USE.md` CU6):
`WindowRequest::PickFile` asks the session to browse on the app's
behalf, and the engine keys the pick to the attested window owner,
enforces one pending pick per window, and requires exactly one
conclusion — a `WindowEvent::FilePicked` carrying the kernel's one-shot,
recipient-owner-bound `fd_grant` delegation handle, or a
`WindowEvent::PickCancelled` — per accepted request. The picker UI is a
session-owned window driving the same shared `lib/browse` engine as the
files app; the requesting app receives only the redeemable handle, never
a path or any browsing authority of its own.

## Tests

`cargo test -p tairix-wm` runs the headless suite against a virtual
framebuffer: premultiplied-alpha correctness (fully-opaque and
fully-transparent edge cases), per-region alpha blending, rounded-corner
masking, z-order and raise, window move/hide/remove with damage repaint,
channel-order encoding, the `Display` present seam, the desktop layer
(drawing over the background and under every window, a smaller-than-screen
layer leaving the background showing, and installing/replacing/clearing
each damaging exactly its footprint), the screen reveal (a full reveal's
frame byte-identical to one the reveal never touched, a half reveal
scaling every composed pixel by the crate's own rounding while the back
buffer keeps the true colour, a zero reveal presenting black, the
premultiplied invariant at every strength, whole-screen damage on a change
and none on a repeat, the layer path declining mid-fade and resuming after
it, and a mode change keeping a fade in flight), the accelerated
layer-encoding present path (background + desktop + window layers,
hidden-window omission, and the over-budget / over-size / backdrop-blur
software fallbacks), the composited backdrop blur (a spread backdrop, a
no-op at radius 0 and for an unknown or hidden window, confinement to the
window rectangle, the logical radius following the output scale, rounded
corners left alone, and a change behind a frosted window repainting it to
exactly the pixels a whole-screen composite gives — the frost's own
identities are pinned in `lib/raster`), and input routing (hit-testing,
click-to-activate focus and raise, desktop-clears-focus,
`DesktopPointerMoved` carrying no position of its own and `DesktopKey`
reporting focus-on-desktop, move-grab drag, and the fail-closed grab edge
cases).
