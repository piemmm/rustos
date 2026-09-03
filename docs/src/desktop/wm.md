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
   a window whose pixels have gone keeps every other property and shows its
   own plate (see [The client plate](#the-client-plate)) until its app
   presents again.
2. `Compositor::composite` walks the damaged screen regions and, for
   every dirty pixel, blends each covering window *over* the opaque
   background, bottom-to-top in z-order, using the Porter–Duff *over*
   operator (`Pixel::over`). It returns the `Region` it actually
   recomposited (screen-clipped), which is what the present step moves.
   Each blend rounds at that pixel's own share of the shared ordered
   dither (see [Dithered blending](#dithered-blending)); an opaque run is
   copied rather than blended and pays nothing for it.
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

## Spread across the machine's cores

A composited row writes one row of the back buffer and the scan-out bytes of that
row, and reads only immutable window content — so the rows of a dirty rectangle are
independent by construction, and bands of them can be composed at the same time on
different cores. `Compositor::set_job_runner` is where an embedder hands over the
worker pool that runs them (`lib/parallel`); the default runs every band on the
calling thread, which is what a single-CPU machine, a headless build, and a process
the kernel would grant no thread all keep.

Splitting changes wall-clock cost and nothing else. The composed pixels are
bit-for-bit what one thread produces whatever order the bands run in, which the
compositor's own tests assert by composing each scene twice — once whole, once split
into bands that run backwards — and comparing the scan-out frame, the back buffer,
and every counted pixel of `FrameStats`.

Two things decide whether a rectangle is split at all:

- **A band must be worth its hand-off.** A dispatch costs a wake syscall and the
  workers' park syscalls, so a rectangle below one band's pixel budget is composed
  on the calling thread with no atomics. That is why a pointer-motion repaint of a
  few rows costs what it always did, while a full-screen recomposite goes wide.
- **The backdrop blur splits differently.** A frost's pieces are column divisions
  of the window's rectangle, because the vertical pass slides a window down each
  column; the mix back over the surface is split by *rows* instead, since that is
  what it writes. Each piece re-primes its sliding window at its own first column,
  which costs `radius` samples per row the undivided pass paid once — so a frost
  asks for exactly one piece per participant rather than several.

Measured on a development host (`cargo xtask bench`), dividing a pass eight ways
while still running the pieces on one thread — which isolates what dividing costs
from what running elsewhere saves — leaves an opaque or translucent full-screen
composite within 1% of undivided, and a full-screen backdrop blur 2.6% behind. That
is the price the other seven participants are bought with.

### The one whole-window pass above the compositor

An application's `Present` hands the session a frame of straight-alpha bytes and
declares which rectangle it changed. Converting that rectangle into the window's
own content surface is the third pass a repaint costs above the compositor, and
it is the one the session **cannot** make smaller: the app declares the damage,
so a client that repaints everything makes the desktop convert everything. It is
therefore spread across the same participants a composite uses —
`Compositor::job_runner` reads the installed runner back, so the conversion and
the composite share one answer about how wide the machine is rather than two
installations that could drift.

Both directions of that conversion are one definition,
`tairix_display::winframe`, beside the channel-order decision the scan-out path
already owns: `encode` is what an app writes its surface out through and `decode`
is what the session reads it back in through, reporting the sub-rectangle whose
pixels genuinely changed. An app passes the calling-thread runner rather than a
pool, and the asymmetry is deliberate: an app decides how much it presents and
should present only what it changed.

Within a row, the columns between two copied opaque runs are one **segment**,
and a segment is composed a *layer* at a time across its whole width — the
base fill, the desktop row, each window row back to front, then the cursor —
not a column at a time through the whole stack. Each layer is a straight run
of source pixels at a screen column and a constant opacity, laid through
`lib/raster`'s one span composite (`blend_span`), which is the same walk
`Surface::blit` takes: one blended pixel is the same arithmetic wherever it
comes from. The order a pixel sees its layers in is unchanged and the frames
are byte-identical; what the segment saves is the per-column dispatch around
the arithmetic, which measurement showed was the larger half of a translucent
composite. Full-screen opaque composition fell from 2.98 ns/px to 0.61 and the
translucent case from 10.04 to 5.99; inlining the per-pixel operators and
hoisting the span's column counter out of that walk took them on to 0.52 and
5.69. Rows where coverage genuinely varies per column — a rounded corner's
arc, and the cursor — keep the column-by-column walk inside their own
contribution.

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

## Dithered blending

A blend into the 8-bit back buffer cannot hold what it was given: a source
of alpha `a` admits only `256 - a` of the 256 levels the picture beneath it
held. Round every pixel the same way and a smoothly shaded wallpaper under a
translucent window — or under a frosted panel — collapses into wide flat
stripes with a hard step between them. It is invisible on a small test
display and unmistakable on a 1080-row screen.

Every blended pixel therefore rounds at its own bias from
`tairix_raster::DitherRow`: values between two levels land on the lower one
in some pixels and the higher one in others, and the area mean carries the
fraction. The row is resolved once per **screen** row, and a span composite
resolves the eight biases of its own first column once more (the pattern's
period is eight) rather than deriving a screen column per pixel;
`WindowRow::sample` scales window opacity and
rounded-corner coverage at the same bias, so a translucent window's own
gradients survive too. This is one shared definition with the rest of the
rasteriser — the login screen's entry veil, a translucent plate, and
`frost_region`'s mix-back all round the same way — not a compositor-local
trick.

Three properties make it safe on the frame path:

- The bias is a pure function of the screen position, so a recomposited
  rectangle matches the frame it replaces exactly, two damage rectangles
  that meet cannot seam, and a still frame never shimmers.
- The tile's mean bias is plain nearest rounding, so nothing lightens or
  darkens and no pixel is ever more than one level from the undithered
  answer.
- Opaque runs are copied whole, so the cost falls only where a blend was
  already being paid: one mask and one load per blended pixel.

## Screen reveal

`Compositor::set_reveal(strength)` scales the whole screen toward black:
`u8::MAX` is the normal picture, `0` is black. It is what the desktop
session fades in over on start-up (see [the session](session.md)), and
nothing else in the compositor reads it.

It is applied at exactly one point — the step in `compose_span` where a
composited pixel becomes a scan-out byte — so every present path
(`composite`, `present`, the accelerated path's software
fallback) dims each pixel exactly once. The back buffer deliberately keeps
the **true** composed colour: a frosted window samples the backdrop out of
it, and a blur-split rectangle re-reads it for its second segment, so
dimming the buffer instead would dim those pixels twice.

The scaling reuses `div255`, multiplies only `r`/`g`/`b` and leaves alpha
untouched, so the premultiplied invariant (`channel <= a`) holds at every
strength and the screen fades to black rather than to transparent. A full
reveal short-circuits to the pixel itself, so a screen nobody is fading
costs one comparison per encoded pixel and is byte-identical to one that
never heard of the reveal. Setting the strength already in force damages
nothing. A mode change carries the strength over, so a session that
re-modes mid-fade keeps fading.

**A fade step re-encodes; it does not recomposite.** Changing the strength
changes every presented pixel, but it changes no *composed* pixel — the back
buffer is bit-identical between two fade steps. So a change of strength goes
into a channel of its own (`mark_scanout`), and `composite` encodes those
rectangles from the back buffer as it stands: no root fill, no desktop layer,
no window, no cursor and no frost. Everything the composite pass touched it
already encoded at the current strength, so those rectangles are subtracted
first and no pixel is encoded twice; the frame reports both as one region, and
`has_damage` answers for both.

That is what makes a screen fade cost what it changes. Measured over a
1 024 000-pixel screen (`cargo xtask bench --filter composite`, `fade step`),
one step falls from 997 µs to 490 µs over an opaque stack and from 2.81 ms to
490 µs over a translucent one or one with a backdrop blur — and the cost stops
depending on the scene at all, because a fade no longer composes it.

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

`Compositor::repaint_desktop(area, paint)` repaints it in place: the owner
keeps the screen-sized buffer it already has (a repaint costs a paint, not a
multi-megabyte allocation) and `paint` receives the surface together with the
rectangles of `area` clipped to the layer, painting inside them and nowhere
else. Only those rectangles are marked, and that matters far more than the
paint itself: the desktop is the **bottom** layer, so marking all of it
recomposites every window above it and throws away every frosted backdrop over
it. An icon taking the hover must cost that icon, not a screenful of blur. A
layer that is absent or sized for a screen this output no longer has is
allocated fresh and painted whole, since it holds no pixels a partial paint
could preserve; a heap that will not give one back leaves the desktop exactly
as it was rather than blanking it.

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
the frosting is missing from. A **translucent window** takes it as well
(`Compositor::has_translucent_window`): the engine would blend that layer
over what is beneath it in the scan-out's own 8 bits with a fixed rounding,
which is exactly what bands a picture under a translucent field, and no
layer stack can express a per-pixel dither. A window's own anti-aliased
corner is not this case — partial coverage a few pixels wide has no gradient
to band — so ordinary rounded windows still take the hardware path. An
**in-flight screen reveal** takes it too:
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
half the shorter side (`round_rect_radius`, the same clamp the coverage
applies). The taskbar's rounded edges reuse this same path rather than a
second implementation (`AGENTS.md` §2.2) — and because the bar floats clear
of the screen edges it faces (`Metrics::taskbar_margin`), all four of its
corners now round against wallpaper rather than two of them against the
screen edge.

`Window::shape` is the one silhouette a window is cut to, whichever kind it
is: a **decorated** window takes its frame's rim radius (`WindowFrame::rim`)
and a plain one its own corner style, and either way the extent is the outer
rectangle. Both the window's own pixels and the frosted backdrop confined to
its rectangle weight themselves by it.

A rounded silhouette leaves a curve where a rectangle would have a corner, and
an application's rows are square, so the window manager keeps content out of
that curve two ways:

- **The client is clipped to the frame's plate.** A decorated window's client
  pixels are cut to the rounded plate the frame fills inside its rim
  (`FrameRim::plate` — the concentric inset and radius the frame itself draws
  from). A pixel the plate does not fully cover belongs to the frame, whose own
  arc — the rim and the plate behind it — is what the curve is made of, so
  content can neither draw over the rim nor square off the corner. A plain
  window has no rim and no furniture to give way to, so its coverage
  anti-aliases its own edge exactly as before.
- **A corner row is furniture over its whole width.** The top and bottom
  furniture strips reach at least the rim's radius even where the reserved band
  is thinner than that, and the side strips take only the rows between them, so
  the frame's own render supplies every pixel of a corner row. A window whose
  content has been released still draws its curve.

The title bar draws no ground of its own: the frame has already laid its plate
under the whole window, rounded, so filling the band again would square off the
very corners the rim curves around — in the colour that is already there.

## The client plate

**A decorated window's client rectangle is always fully covered.** The
client's own pixels cover as much of it as they extend to; every remaining
column and row is the frame's body colour (`Palette::surface`) — the same
plate the frame lays inside its rim, resolved once per window with its band
(`Window::refresh_band`) and laid a run at a time on the composite's fast path
(`blend_solid_span`). An *undecorated* window is nothing but its client, so it
has no plate and draws nothing where it has no pixels.

This is what keeps a window one object while its client is behind:

- **A live resize-grab.** The window manager reaches the new outer rectangle
  on the sample the pointer moved; the client learns its new size, re-renders,
  and presents a round trip later. Without a plate the strip between the two
  was the desktop, showing through the middle of a window — the frame visibly
  running ahead of its own interior.
- **A client that presents short of its frame.** An app that rounds its own
  size down — a terminal snapping to whole character cells — presents a
  surface narrower and shorter than the reserved client area. The residue is
  plate, so the content still meets the decoration with no gap.
- **Released or unanswered pixels.** A window whose content went back under
  memory pressure, or whose app ignores the redraw request, reads as an empty
  window rather than a hole onto the desktop.

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

The desktop's own chrome is the standing consumer: the session places the
taskbar, every popup it opens — the program-library launcher, the hover window
picker, the notification popover, the Switchboard capsule's readout — and every
surface of an open menu chain with the theme's `chrome_backdrop_blur`, because
each is drawn on a translucent ground that only reads as frosted glass over a
blurred backdrop (`plans/GUI-CONTROLS-DESIGN.md`, "Surface ground"). It is asked
for as the surface is placed, so a chrome window is never shown for a frame
wearing the frosting of whatever was placed before it. A desktop session
therefore always has at least one frosted window, which is why the accelerated
layer path — a hardware plane cannot read what is beneath it — currently falls
back to this software composite for the whole frame
(`plans/FIX-DISPLAY-ACCELERATION.md`).

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
selected account tile draws through too. It reads the rectangle out of the
back buffer a row at a time, blurs it, and mixes the blurred pixels back over
the originals at a per-pixel weight the caller supplies.

The blur is a **separable box blur**: a horizontal pass then a vertical
one, each carrying a running sum so the window slides by one add and one
subtract per output. The cost is proportional to the rectangle's *area*
whatever the radius, never to area × radius. Nothing is written until both
passes are done, which is what lets the horizontal one read the surface
directly instead of a copy of it. The blurred pixels and the pass-to-pass
intermediate live in one `tairix_raster::BlurScratch` the compositor owns,
grown to the largest frosted rectangle the session has needed and reused,
so a frosted window allocates nothing after its first frame; a mode change
releases it rather than pinning the old screen's worth of pixels.

`Surface::frost_region_around` frosts the same rectangle *except* a kept
inner block, writing exactly the pixels the whole-rectangle frost would write
around it. The kept block is either a retained frost's still-valid core or a
core no output channel could record (see [A frost no output channel can
record](#a-frost-no-output-channel-can-record)). The rectangle still decides the answer — samples replicate at its
edges and coverage is read at its own coordinates — so a border is never a
smaller frost of a smaller rectangle, which would spread a clipped
neighbourhood and seam against the pixels it was kept beside. The border's
bands are all blurred before any is mixed back, because a band's own
neighbourhood reaches into the bands next to it and what it must read there is
the backdrop, not the frost of it. This is what a dragged frosted window costs
instead of a whole blur (see [Retained
backdrops](#retained-backdrops)).

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
  **no position of its own**: the seat owns the pointer position, which is
  where the desktop layer's owner reads it from, so the response need not
  duplicate it. The router takes
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
- **A dragged window keeps a grabbable patch of its title bar on screen.**
  The grab captures the bar's move surface — `TitleBarLayout::drag`, the
  span between the two command clusters — in window-local coordinates, and
  `clamp_move_origin` keeps that span inside `screen_rect`: the whole band
  vertically, and sideways at least a patch as wide as the band is tall. A
  window may still hang off any edge, which is normal on a desktop; it
  cannot be pushed somewhere with nothing left to drag it back by. The
  region is the whole framebuffer, so a single big desktop spanning several
  monitors is one region and a window may straddle two of them. An
  undecorated window has no move surface and is not clamped.
- **Furniture hover** — pointer motion that lands on a window's decorations
  is handed to that window's frame (and the frame the pointer just left is
  told too, so its highlight goes out). Only the window manager ever sees a
  pointer over its own decorations, so without this a command button could
  never light up under the pointer. The response is still `Ignored` — no
  client owns the motion — and the frame reports its own repainted
  rectangles, so a sample crossing the drag region costs nothing and one
  entering a command repaints that command alone. A hover that ends because
  the *seat* took the pointer away, rather than because it moved, ends through
  `set_pointer_focus(Left)` → `Compositor::frame_pointer_left`, which is
  position-independent for the reason above.

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

When the frost does **not** have to be recomputed at all there is no
neighbourhood to spread and no promotion either: the retained frost is copied
back and the damage stays the rectangle it was marked as. A frost that survives
only in part is promoted like one that must be blurred outright, because its
border is blurred. See [Retained
backdrops](#retained-backdrops).

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

**An embedder-painted window declares its own damage.**
`Compositor::repaint_window(id, size, area, paint)` is the window-content
mirror of `repaint_desktop`, for the surfaces the session paints itself — a
menu plate, a session panel. The window keeps the buffer it already has and
`paint` receives it with the rectangles of `area` clipped to it, in the
surface's own local pixels; only those are marked. The damage is declared
rather than discovered because an embedder painting its own model knows what
changed before it paints, where a client does not. A window whose content is
absent or is not `size` is given a fresh buffer and painted whole, and its
geometry follows that size; an unknown window, or a heap that will not give a
buffer back, changes and damages nothing.

The painter's obligation is the same as the desktop layer's, with one extra
edge: it must lay its own background over each rectangle, and where that
background is *translucent* and *rounded* it must clear first. A laid colour
replaces a pixel the shape fully covers but mixes an arc pixel toward it by
that pixel's coverage, so a corner would otherwise keep a tint of whatever the
previous paint left there. `MenuChain::render_surface` does exactly that, which
is what makes a plate's partial repaint land the pixels a whole one would.

**A window cannot be dragged smaller than its own furniture, or than its
application declared.** `Compositor::window_min_outer_size` is the greater
of two real floors, and an interactive resize captures it at grab start:

- the *furniture's* floor, `WindowFrame::min_outer_size` — a band wide
  enough to seat all four commands with one command's worth of drag surface
  left between them (`TitleBar::min_band_width`), and the bands plus one
  standard control of client in height. It holds for every decorated
  window, including one whose application declared nothing.
- the *application's* declared minimum client extent, adopted through
  `Compositor::set_window_min_client_size` from what the app stated when it
  created its window. Without it an app that cannot lay out below some size
  resizes itself back up while the drag keeps shrinking, and the two fight
  once per pointer sample — the window and its content visibly bouncing.

The floor bounds a *user* resize. An application sizing its own window is
choosing that size, so `resize_window_client` is not clamped and a window
already smaller than the minimum is never grown under its owner.

**The frame is the window manager's; the pixels are the client's.** A
window's content buffer is sized by the frame the *client* presents, never
by the window manager's own resize: `resize_window` and
`resize_window_client` move the geometry the compositor draws and lays the
furniture out from (`Window::client_size`) and do not touch the buffer, and
`present_window_content` establishes the buffer whenever the one held does
not describe the presented frame. This is what makes a live resize correct.
A resize-grab moves the frame on every pointer motion and tells the app its
new size on every one too, so its content is resized with the frame rather
than stretched until the button comes up. Reshaping its buffer under it would
refuse each present it makes in the meantime, which an app cannot tell from a
dead session; instead the compositor simply draws the part of the buffer that
lands inside the client area (`Window::row`) and fills the rest of that area
with the window's own plate (see [The client plate](#the-client-plate)), so
an app a sample behind is a sample behind rather than a refused frame — and
never a gap between its content and the decoration around it. A buffer
established afresh carried nothing over, so the whole client area is marked
dirty rather than the rectangle the conversion reported.

**The drag owns the geometry for its whole duration.** It recomputes the
outer rectangle from the captured start and the live pointer on every sample,
so `WindowHost::window_resized` accepts the app's re-map *without* moving the
window while a grab is live (`InputRouter::resizing` names the grabbed
window): adopting the size the app re-mapped at — necessarily a sample stale —
would set the window back to wherever the app had got to, and the two would
fight once per pointer sample. The settled size goes out when the grab ends,
and the app's `Resize` moves the geometry again from there.

**A resize to the geometry already in force costs nothing.** `resize_window`
and `resize_window_client` accept it — the drag has to be told its window is
alive, and a refusal ends the grab — but mark no damage and keep the window's
rendered furniture, exactly as a move to the current origin does. Most samples
of a drag do not move the grabbed edge: the pointer wanders along the axis the
edge does not follow, and a drag held at the window's minimum recomputes the
same rectangle for as long as it is held. Repainting on the *request* rather
than on the *change* re-rendered a whole window's furniture and recomposited
it, per sample, for a geometry nobody could see change.

**A run of resize reports folds to its newest, three times over.** A client
extent is a value the window converges on, not an occurrence it must witness.
The session's input drain keeps only the last `Resized` of an unbroken run
over one window (`DesktopShell::pump`) — it forwards the window's *current*
extent, and the whole batch is applied before any outcome is forwarded, so
every earlier sample of the run would carry the size the last one settled on.
Behind that, the hold-back overwrites a held `Resized` where it stands (see
[the desktop session](./session.md)) and the shared client reader
(`tairix_window::WindowEvents`) drops the stale ones it has already been
sent. An app slower than the pointer therefore lags a frame, never a queue of
re-maps for sizes the window has already left.

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
- **A frame is presented once, naming everything it changed.** Damage
  that covers the screen is one `Display::present`; anything else is one
  `Display::present_rects` carrying the frame's disjoint dirty
  rectangles. The bytes moved are proportional to what changed rather
  than to the box around it — a dirty taskbar strip along the bottom edge
  plus a cursor near the top is two small blits, not a near-full-screen
  one — and a scattered frame costs one round trip, not one per
  rectangle.
- **Covering the screen and spanning it are different questions.** Two
  far-apart corners have a bounding box the size of the screen while
  changing a few dozen pixels, so the whole-frame path is reserved for
  damage that really is the surface. `tairix_display::damage_list` is the
  one place that decides, and the client's frame ring honours the same
  distinction: a buffer catching up copies the rectangles it missed, not
  the box around them.
- **Past `MAX_DAMAGE_RECTS` rectangles the list degrades to its bounding
  box** — over-covering costs pixels, dropping a rectangle would leave
  stale ones on screen. It is still one present: the bound is what one
  message carries, not how often a frame may publish.

## Retained backdrops

A window that is **translucent, backdrop-blurred, or both** is composed over
the picture beneath its rectangle, so a frame that touches it otherwise
recomposes that whole stack. That backdrop is a function of exactly four
things: the pixels the layers beneath it
composed, the window's own rectangle, its physical blur radius, and the window
shape the blurred copy is mixed back through. It is **not** a function of
anything at or above its own layer — the blur happens before the window's own
translucent pixels are blended over it, and everything stacked above is blended
afterwards. The pointer moving inside a frosted terminal and a window dragged
across one, the two dominant interactions on the desktop, therefore change
nothing the frost reads, yet either used to re-blur the whole window per sample:
two separable passes over every pixel of it, measured at 17.4 ms for a 64×24
repaint against 0.9 µs for the same repaint over an opaque stack. Dragging the
frosted window *itself* changes nothing beneath it either, and that is the third
case the cache answers.

The compositor keeps each frosted window's backdrop instead, in a bounded,
pressure-governed cache on the same terms as the window furniture above:
ceilinged at one screenful of pixels, released when the memory-pressure band
tightens, and **wiped** on release,
because a frost is a blurred image of whatever the user had on screen — and an
unblurred backdrop is a plain one, so the wipe matters more, not less. The same
repaint now costs 26 µs.

A plainly translucent window — no blur — is the same problem without the two
blur passes, and takes the same path: a blur of radius zero leaves the composed
layers exactly as it found them, so the retained entry *is* the composed
backdrop and `Window::reads_backdrop` is the one predicate deciding which
windows are worth keeping one for (a whole-window opacity below full, or a
blur). There is no second cache and no second copy of the reuse rule. Dragging
a translucent unblurred terminal fell from **19.89 ns/px** a pointer sample to
**6.95** — it was the slowest window on the desktop to drag and is now cheaper
than a frosted one, which itself fell from **15.30** to **9.30**. A backdrop is
snapshotted with `Surface::overwrite`, a row copy, rather than composited onto
a blank surface.

An antialiased corner is deliberately *not* enough to retain a backdrop for:
its backdrop is a few pixels of arc, not a field. Nor is a client that paints
alpha into its own content, which cannot be known without reading every pixel
of it. Both still composite correctly — they blend the layers below instead of
copying a retained picture of them.

How a retained backdrop is known to be still right:

- **The rectangle, the radius, and the shape are recorded in the entry** and
  consulted on every lookup, so the check holds even if the compositor forgot
  to say anything had changed. A radius that differs keeps nothing: every pixel
  is a different average. Geometry is the interesting case, and it is answered
  by *how much* survives rather than yes or no (below). The rectangle recorded
  is the window's whole one, not the part of it on screen: a window pushed off
  an edge is frosted from the row and column the screen begins at while its
  shape is still read from its own top-left, so two positions that clip to the
  same on-screen rectangle weight the same pixels differently.
- **A window that has moved, resized, or changed shape keeps everything the
  change cannot reach.** Its backdrop did not move, so in *screen* coordinates
  the retained pixels are still exactly what a fresh blur would write wherever
  neither difference between the two positions applies: the blur replicates at
  its rectangle's edges, so a pixel less than the radius inside either
  position's on-screen rectangle averaged a different set of samples, and the
  shape weights the mix at a window-local coordinate, so a pixel within a
  corner's reach of either position's own rectangle was mixed at a different
  coverage. Both are confined to a border, so the shared rectangle taken in by
  the larger of the two reaches is copied and only that border is blurred
  again. Dragging a frosted terminal was the interaction this cache was worst
  at — every sample re-blurred the whole window *and* re-composed the entire
  stack beneath it — and a three-pixel sample now blurs under a fifth of it. A
  jump that leaves no shared core keeps nothing and blurs the whole rectangle,
  so the fallback is the old cost and never a seam.
- **The pixels beneath it cannot be self-checked** without reading the ones the
  copy was meant to save, so the entry is dropped when the compositor marks
  damage that could have changed them. Marking distinguishes three cases: a
  change confined to *one window's own layer* — its content, position, size,
  shape or furniture — drops only the frosts of windows stacked above that one,
  which is why a window dragged across a frosted one costs it nothing; a change
  that is not confined to a layer (the root fill, the desktop layer, the
  density or theme, or a restacking, which changes *which* layers a frost sees)
  drops every frost it reaches; and a change no frost can read at all — the
  cursor overlay, composed after every window — drops none. The screen reveal
  drops none either, and composes nothing: it marks the scan-out channel
  instead (see [Screen reveal](#screen-reveal)).
- **How much of a frost may be reused is asked once per frame and remembered.**
  The recompose plan and the composite that follows it both need the answer, and
  two lookups could disagree — which would leave a window the plan did not
  widen for being blurred over a rectangle whose lower layers the frame never
  composed. That one lookup is also what the cache counts, so a reuse reads as
  a hit and refreshes the entry's recency: the frost every frame serves must
  not be the first one a pressured cache gives back. A frost that survives only
  in part is promoted like one that must be blurred outright, because its
  border is blurred and a border blurred over a strip of damage would spread a
  neighbourhood clipped to that strip.
- **The layers a frost covers are not composed at all.** A frost is copied on
  top of whatever is beneath it, so composing that stack first is work the copy
  throws away — a whole window's worth of blending per pointer sample for a
  dragged terminal. The frame composes the layers below only outside what the
  frost will write over: nothing at all under a frost reused whole, and only the
  ring the border blur reads under one reused in part. What is left is composed
  as the disjoint rectangles it is, never as the box around them.
- **A frost the frame recomputed any part of is captured whole**, so the next
  frame compares against where the window is now rather than eroding the same
  core until nothing is left of it.
- **Recomputing one frost drops any frost above it that overlaps**, because a
  blur spreads the change far past the rectangle that caused it, so the window
  above reads different bytes even where the damage never reached.
- **A density or mode change empties the cache** through its epoch. Both are
  already caught per entry, so this is not what keeps a stale frost off the
  screen — it is what stops a superseded one *staying charged* against the
  budget until it is next looked up. A window that stops frosting altogether is
  never looked up again, so setting its blur radius to zero releases its entry
  outright rather than leaving a screenful of dead pixels charged.

### Frosting is rationed, front to back

A frost is a *whole window's* rectangle, and stacked frosted windows all read
the same pixels, so `n` of them want `n` screenfuls of retention against a
budget of one. Asking "does one more fit?" of each in turn answers *yes* for
every window in such a stack, so each frame blurred one, evicted another, and
re-blurred it the next frame: cost climbed with the depth of the stack and the
cache served nobody. Sixteen terminals opened on top of one another at the
shipped translucent, blurred default is the shape that finds it, and it took the
desktop to a crawl — one repainted cell costing several screenfuls of blur and
of blending.

`Compositor::grant_backdrops` therefore spends the cache's live ceiling from the
**front** of the stack and stops (`ReclaimCache::holds`, which weighs a whole
set against the ceiling rather than one entry against what is charged):

- What the budget reaches is **frosted** — its backdrop retained, the window
  composed over it — and is recorded on the window (`Window::is_frosted`), so
  the plan and the composite read one answer.
- What it does not reach composites as the **plain translucent window it also
  is**: no blur, no retained backdrop, no segment split, and the layers beneath
  it blended straight through. The frame it draws there is byte for byte what
  the same window draws with its blur turned off, which the compositor's tests
  assert rather than assume.

Depth is bounded by the one fact that matters — what can be retained — rather
than by a window count that a large screen would waste and a small one could not
afford. Frosts that do not overlap all fit, since together they cover no more
than the screen, so an ordinary desktop is never rationed; only a pile of them
on the same pixels is, which is exactly the pathology. Because the frame never
over-commits the budget, nothing is admitted only to be evicted.

Measured on that cascade as a host unit test — sixteen 80%-opaque blurred
terminals on eight positions over a 1024×768 output, retained backdrops wanting
some thirteen screenfuls against a budget of one — the first frame blurs 640 680
pixels (four fifths of the screen) and three of the sixteen windows are frosted
within the ceiling. A terminal then repainting one cell of itself blurs **0**
pixels and recomposes **1**, where the same repaint over an ungoverned stack
blurred some 4.7 M and blended some 4.9 M.

What it costs is that a window buried under a pile of frosted ones reads as
translucent rather than as frosted glass where it still shows. That is the
deliberate trade, and it is bounded: nothing is ever drawn wrong, only less
prettily than an unbounded machine would draw it.

**A change of mind marks damage.** The ration turns on the cache's ceiling and
the live pressure band as well as on the scene, so a window can start or stop
frosting with nothing on screen having moved — and it draws differently either
way. The grant runs before the frame takes its damage and marks the bounds of
every window whose answer changed, releasing the retained entry of one that has
stopped frosting at the same time, so the windows still frosted are weighed
against a budget the refused ones have given back.

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
every one. The partial reuse a moved window takes is held to the same bar from
both ends: that sweep covers it, and `lib/raster` proves separately that
frosting a border around a kept block is bit-for-bit the whole frost, over
random blocks, radii and coverages.

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
| `dirty_rects` | Dirty rectangles the frame redrew, whether by recomposing them or by encoding them afresh from the back buffer (a fade step). |
| `present_calls` | Calls the frame made into the display driver to publish itself — the round trips of the section above. |
| `chrome_hits` / `chrome_misses` | Window-furniture lookups served from the retained cache versus rendered again, whether the cache then kept them or refused. |

The counts are independent of how the frame was divided: each band tallies its own
work and folds it in once it is done, so a band never touches shared state while it
runs and a split frame reports exactly what a whole one does.

The reading that matters is **damaged vs blended vs screen pixels**: damage
far below the screen means the damage tracking is working, and blends far
above the damage means the frame is compositing depth the user cannot see.
`FrameStats::is_idle()` distinguishes a frame that changed nothing from one
that changed a little, so a wake that did no work reports *idle* rather
than a row of zeros pretending to be a frame. A fade step is the honest
extreme of that reading: it damages and encodes the whole screen and blends
none of it, because the composed pixels it presents are the ones already
there.

### And what every frame cost (`frame_totals`)

One frame's counts are a live gauge; a *gesture* needs the run of them.
`Compositor::frame_totals` returns the same accumulator's since-epoch
`DesktopFrameTotals`: the counts above summed over every frame composed
against the current screen, plus the worst single frame's damage and blends.
The peaks are why the aggregate is not simply an average — a hover that
repaints one control and one that repaints the screen have similar means and
very different worst frames, and the worst frame is the regression the damage
tracking exists to prevent.

Each frame is folded exactly once, when the next one opens, and the frame
still in progress is added to a *copy* on read — so reading the totals twice
gives the same answer, which a reader on a frame path relies on. A
display-mode change starts a fresh epoch: every pixel figure is read against
`screen_px` as its denominator, and counts taken against a different screen
answer a different question.

**A wake is not a frame.** `Compositor::present` opens one only when there is
damage pending, so a run loop calling it on every wake — which the session's
does — leaves the totals untouched while the screen is idle. That is what lets
a reader use them as a change signal: the session's own publisher (below)
republishes exactly while they move, and counting idle wakes as frames would
have left it doing so for ever on a desktop nobody was touching.

`DesktopFrameTotals` is an ABI record because it leaves the process: the
session publishes it to the System Information API, where a monitor, a shell
(`sysinfo frames`), or a regression gate reads it under
`CAP_SYSINFO_GLOBAL` (`docs/src/abi/sysinfo.md`). The decoder there refuses
counts no composite pass could produce, and the compositor's own tests
round-trip its fold through that decoder — a producer the receiver would
reject is a defect on this side.

**A gesture is judged on a bracketed window, never on the epoch.** The peaks
are the right reading of *a gesture's* frames, but the published epoch begins
at the session's first frame and bring-up legitimately composes full-screen
ones — the wallpaper and the reveal fade — so the epoch's peak is bring-up's
and its mean is too. A gate therefore samples the record either side of the
gesture and judges the difference, whose counters are all work rather than
time and so are load-independent.
`tests/integration/desktop_hover_qemu_aarch64` is that gate: it boots the
production aarch64 graphical session, launches the `framestats` fixture from
the program library to take a sample, sweeps the pointer the length of the
icon bar, and launches it again. The fixture is what makes the reading
reachable at all — the query is userland IPC, which a freestanding test kernel
cannot issue, so the fixture asks and re-emits the counters through the system
log, where the guest decodes them. The window is then held to per-frame damage
as a share of the screen, overdraw per damaged pixel, bounded one-off frost
work, no re-rendered furniture, and no more driver calls than rectangles and
frames.

## Server-side window decorations

Window decorations — a title bar carrying the four command controls in two
corner clusters (put-to-back and close at the leading edge, minimize and
size-toggle at the trailing one) with the identity and title left-justified in
the span between them, and the frame rim — are drawn by the **window manager**,
never by an app (`AGENTS.md` §10, `plans/GUI-CONTROLS-DESIGN.md` §1,
§11.17–§11.23; `plans/COMPOSITOR-WORK.md`). An app supplies only its content
surface and typed window metadata; it can neither paint over nor receive input
from the chrome. The furniture family itself lives once in
`lib/controls::window` (`WindowFrame`, `TitleBar`, `WindowControl`,
`ResizeGrabber`) and is composed here, so there is no second visual recipe
(`AGENTS.md` §2.2). A command is *bar-seated*: it wears no perimeter of its own
in any state and no plate at all while it rests, so it states hover and press on
its plate alone and the row reads as part of the bar
(`plans/GUI-CONTROLS-DESIGN.md` §6). Each of the four lights up in its **own**
hue — red to close, yellow to minimize, green to the size toggle, blue to
put-to-back — so the pointer landing on one says which command it is about to
fire before the glyph is read. The wash is authored at half opacity and resolved
against the window body, so the title bar tints rather than being covered; a
press deepens it, keyboard focus still states itself on the ring inside the
plate, and a denied or disabled command reads as denied or disabled rather than
as its colour. See [theming](./theming.md) for the four roles.

- **A command cell is a square that carries no margin.** The cell *is* the
  button: it fills the band's height, it is as wide as it is tall, it touches
  the cell beside it, and the outermost one in each cluster is hard against the
  band's end. So a hover lights every pixel between one command and the next
  and a press lands anywhere in the cell — where the older layout centred a
  narrow upright slot in the band with gaps around it, leaving strips where the
  highlight dropped out and a click did nothing. A cell's width is the band's
  own height rather than a metric of its own: two numbers that have to be equal
  for the cell to be square are one number too many, so `title_bar_height` is
  the only thing that sets it.
  The two spacings that remain are not button margins: one holds the identity
  group off the commands, the other its title off its icon. Where a cell meets
  the band's end the window's rim curves through it, so that one corner curves
  with it (`BandCorner`) while the other three stay square — a cell rounded on
  every corner would read as a floating tab rather than part of the bar. The
  plate is drawn larger than its cell in the directions whose corners must stay
  square and clipped back to the cell, so one rounded-rectangle fill yields
  exactly one rounded corner and the window's silhouette is never squared off.

- **The band carries its application's hue.** A decorated window whose identity
  icon has a discernible colour washes its title band with it: strongest at the
  icon and fading out in both directions over `Metrics::title_hue_reach` (500
  logical pixels), across the full band so it runs *behind* the commands on a
  short bar, at `Palette::title_hue_alpha` (a little under a third) so it reads
  as a tint on the chrome rather than a second, blurrier copy of the icon. A
  reach rather than a width: a wide bar keeps its far reaches plain instead of
  stretching one ramp ever thinner, and a narrow one is tinted end to end. An
  unfocused window's icon greys out but its hue only *partly* desaturates — it
  is the last thing still saying which application owns that window, and a
  desktop of identically grey bars reads worse, not calmer. Greyscale or absent
  artwork lends no hue and the band stays plain. The dominant colour is resolved
  once, by `Surface::dominant_color` where the artwork is installed, never
  re-read per repaint; the wash itself is `Surface::wash_region` under a mask
  that multiplies the fade by the rim's own arc, so a band drawn corner to
  corner cannot square off the window either.

- **Reserved band (geometry).** `Compositor::set_window_frame` attaches a
  `WindowFrame` and reserves a furniture band *around* the client from the
  frame's `FrameInsets` at the active `Scale` and `Theme`: the window's
  outer `bounds` grow to hold the decoration and the content surface is
  presented inset at `window_client_rect`, so the client never receives frame
  input. That band is the border plus the title bar above and the **thin
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
  roughly a thirty-fifth of what one outer-sized surface did.
  **Nothing outer-sized is allocated on the way there either.** Each strip is
  a `Surface` the size of its own band that the whole frame paints *into*,
  standing in for that band's rectangle of the window
  (`Surface::with_origin`): the frame draws across the outer rectangle in the
  window's own coordinates and every write outside the band is off the
  surface and dropped. So the largest buffer a chrome render asks the
  allocator for is one band, not one window — which matters because the
  render is per *cache miss*, and the cache is ceilinged at a screenful and
  reclaimed under pressure, so a cold screenful of decorated windows used to
  re-pay a window-sized transient each. A strip is pixel-identical to the
  same rectangle of a whole-window render, because a stated origin places
  every shape, ramp and ordered dither where the drawing says rather than
  where the buffer begins. The band is composed only where the surface
  admits some of it (`Surface::admits`), so a strip the title does not reach
  neither elides its text nor rasterises its identity glyph. The compositor
  samples those strips in the reserved band and the client content inside
  them (`Window::row` /
  `Window::sample_local`), for both the software and the
  hardware-accelerated present paths; a screen row needs at most two of
  them, since a row is either in the top/bottom strip or crosses the client
  between the two side borders. Those two strips are as deep as the rim's
  corner radius wherever the reserved band is thinner (above), so on a corner
  row the frame draws the curve and the client is clipped out of it — which is
  the only way furniture and client share a row, and it reaches no further in
  than the radius. The furniture is animation-free, so it is reduced-motion
  correct by construction, and high contrast thickens the command-glyph
  strokes.
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
- **The resize band is invisible, and it straddles the window edge.** Because
  the drawn band is only the thin rim, a resizable window's resize zones are a
  hit region with no pixels of their own — the invisible resize border macOS,
  GNOME, and Windows use. `GrabReach` resolves the theme's `resize_edge_grab`
  for the left, right, and bottom edges and the wider `resize_corner_grab` for
  the two bottom corners, whose square would otherwise narrow to the edge width
  at its very tip and be the hardest thing on the frame to hit. The corner wins
  over the two edges that form it and is clamped never to fall below the edge
  band.
  - **Centred, not inward-only.** Each band is that thickness measured *across*
    the outer edge: `GrabReach::outward` is half of it (rounding down) and
    `GrabReach::inward` the rest, so the odd pixel goes inward and a one-pixel
    band sits exactly on the window's own outermost pixel. The band stays as
    easy to hit while costing the client half what an inward-only reach would,
    which is what leaves a scrollbar hard against the window edge usable
    instead of mostly swallowed.
  - **A press on the inner half is `ResizeEdge`, not `Client`:** the app still
    draws those pixels but does not receive presses on them, the accepted trade
    for a border that costs no visible space. Since the router consults the
    frame first, that inner strip also wins over a window's root-viewport
    scrollbar furniture. Drawing stays strictly separated even so — the frame
    paints no furniture mark inside the client.
  - **The outer half is resolved against the stack it is in.**
    `WindowFrame::hit` classifies a point outside `bounds` too, but *which*
    window owns such a point is the window manager's call, and
    `Compositor::pointer_target` makes it in **one** pass from the topmost
    window down: each window in turn is asked both "do your pixels cover
    this?" and "does your band reach it?", and the first to claim wins
    (`PointerTarget::Window` / `PointerTarget::ResizeBand`). Stacking order
    therefore decides between the two — a front window's band beats a window
    *behind* it, and no window's band can take a press from pixels drawn in
    front of it. Asking the two questions in separate passes gets the first
    half wrong: it left a window's edge dead wherever its outward half
    overlapped another window, which is what made resizing look broken on
    every window after the first. Only the primary press and the cursor
    consult it; a secondary press, a wheel, and pointer-move still belong to
    the desktop there, so the backdrop menu stays reachable everywhere it was.
  - **The title bar keeps its whole band** — it is resolved first, and the side
    bands are explicitly bounded to start at its foot on *both* sides of the
    edge, so the outward half never claims a column beside a band it does not
    reach on the inside.
  - **The gesture is armed against the frame's grab region, not the window.**
    `WindowFrame::grab_region` (via `Compositor::window_grab_region`) is the
    outer rectangle grown by the widest band's outward reach on the three
    sides that carry one, and it is what the shared `ResizeGrabber` is handed
    as its hit region. Handing it the window rectangle instead refused
    precisely the outward half the hit map had just accepted: the grab
    latched, the pointer kept its double arrow, and no motion moved an edge —
    an affordance that advertised itself and then did nothing, on half the
    area of every band. `hit` remains the gate that decides *which* edge a
    point grabs; the region only has to contain every point it accepts.
  - What makes the invisible border discoverable is the **pointer**: the same
    hit map drives cursor selection, so crossing into the band — inside or
    outside the window — swaps the arrow for the double arrow of the axis that
    edge moves along, and a grab keeps that shape for the whole gesture (see
    [Pointer cursors](./cursors.md)).
- **A secondary title-bar drag moves the window without restacking it.** A
  right-press on the title-bar drag region begins the *same* move-grab a
  primary press does — one clamp, one motion path, one `Moved` response — but
  skips the raise, so the window can be repositioned while it stays where it is
  in the stack. Every other secondary press still raises and focuses. A
  move-grab records the button that began it, so only that button's release
  ends it and a stray click of the other one mid-drag is consumed rather than
  dropping the window.
- **The seat's held modifiers live here.** A modifier key produces no character
  and is no `NamedKey`, so it reaches no surface as a key; the driver reports
  the edge as an `InputEvent::ModifiersChanged` instead and the router keeps the
  current set (`InputRouter::modifiers`). The session stamps it onto every
  `WindowEvent::Pointer` it delivers, which is what lets an app qualify a click
  by a modifier (a shift-click) without shadowing state it could never see.
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
  area (the screen minus the band the taskbar holds, which runs from that
  screen edge to the bar's inner side and so includes the wallpaper margin the
  bar floats above) or restores, returning `WindowEvent::Resized`
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

- **Both sides let go, or nothing is freed.** The compositor's copy is one
  of *three*: the app's render target, the frame region it presents from, and
  this. The pages behind the region go only when the app and the session have
  both unmapped it, so a release the session keeps to itself frees a third of
  a window and leaves two thirds pinned. It therefore unmaps its side
  (`WindowServer::release_frames`) and tells the client
  (`WindowEvent::ContentReleased`), which releases its own two and re-attaches
  a fresh region on the paint that follows the next redraw request
  (`WindowClient::frame_pixels`). A present against a released window is
  refused `NotAttached` rather than reading a mapping neither side has.
- **A window nobody can see is not asked to present.** Asking one would have
  its client establish the buffer the release just freed, for pixels nobody can
  see: the release would free nothing and cost a repaint per hidden window,
  under pressure. The request is made by `set_visible` when the window is next
  shown; a *visible* window released at critical pressure is still asked
  straight away, because it must not be left blank.
- **What survives a release.** Only the pixels go. The window keeps its
  client size (`Window::client_size`, retained independently of the
  buffer), origin, z-order, visibility, furniture, cursor, viewport, and
  size state, so a released window still hit-tests, still draws its title
  bar and borders, still takes focus, and still resizes. Its client area
  composites as its own plate (see [The client plate](#the-client-plate)),
  and everything else on screen is unchanged.
- **Releasing wipes.** The buffer holds user data, so
  `Window::release_content` overwrites every pixel before dropping the
  allocation rather than trusting the allocator to have cleared it.
- **The redraw handshake.** A present carries only a *damage rectangle*,
  so a re-established surface starts transparent — the plate showing
  through it on a decorated window — and is correct only once a
  full-window present arrives. Every release therefore queues a
  `WindowEvent::RedrawRequested` for that window, drained by the embedder
  through `Compositor::pending_redraws`. The compositor never reaches for
  the window protocol itself — the wm crate has no dependency on the
  window-server side — and it queues the same request when a window with
  no content is made visible again. `lib/window` answers the event on the
  app's behalf by re-presenting its last frame with full-window damage,
  so an app that does nothing still gets its pixels back; an app that
  ignores the event simply leaves an empty plate inside its frame while
  the desktop keeps running.
- **Two triggers, one decision.** The ladder reads the band *and* each
  window's visibility, so it runs when either moves: on the band's wake
  (`Compositor::release_content_under_pressure`) and on the hide
  (`Compositor::set_visible`, which applies the same per-window decision to
  the window it just hid). The band's wake alone is not enough, because it is
  edge-triggered: a user minimising a window on a machine whose pressure has
  already settled produces no edge, so the largest block the desktop could
  give back would be released only if the band happened to move again. A
  minimise-then-restore inside one wake withdraws its own undrained notice
  instead, since neither side has let go yet and telling the client would cost
  an unmap and a re-attach that change nothing.
- **The session records the release.** Handing a window's frames back emits
  `CONTENT_RELEASED` naming the window and the bytes
  ([session](session.md#and-its-own-release-witness)); every other reclaim
  decision on the machine is logged, and this is the largest of them.
- **The pressure ladder** (the per-window decision both triggers share):
  - **Normal** — nothing is released. There is no reason to make an app
    repaint while memory is plentiful.
  - **Mild and deeper** — every **hidden or minimised** window's content
    goes, whether it was already hidden when the band moved or is hidden
    afterwards. Nobody is looking at it, so the release is invisible and the
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
the group that is left-justified in the span the two command clusters leave
between them, with the title text after it. The icon is inert: it drags the
window like the rest of the band and is never a control. A window with no
identity reserves no slot and its title takes that leading edge on its own.

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
- **The same identity dresses both surfaces.** That one resolution gives the
  window its title-bar icon *and* its taskbar entry its icon
  (`TaskList::set_artwork` through `DesktopShell::set_task_artwork`), so a
  running application is recognised on the bar by its own icon — pinned or
  not — and the two surfaces can never name different applications. The
  bundle's manifest is read once and rasterised per slot, because the slots
  differ only in pixel side.
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
  only that window's chrome-cache entry; `Taskbar::task_icon_side` reports the
  bar's own slot side the same way. A second window of the same application
  costs a cache lookup for each: the shared `ArtworkCache` serves both without
  re-reading or re-decoding the asset.
- **Greyed while the window is not focused.** The icon is drawn desaturated by
  activation — nearly all of its colour on the active frame, none of it on an
  inactive one — so an unfocused window's icon reads as quiet as its muted
  title. The reduction happens as the artwork lands
  (`Surface::blit_desaturated`, the one saturation definition in `lib/raster`),
  so the session still caches one full-colour icon per (bundle, pixel side) and
  an activation change re-draws it rather than re-rasterising it.

The title text elides with the shared `ELLIPSIS` mark rather than being cut,
because a title may be a path. The group keeps that leading edge at every
width, so the icon and the start of the title stay put as the window narrows
and only the tail is marked.

An app retitles its own window over the channel with
`WindowRequest::SetTitle`, which the server admits only from the window's
owner; `ShellWindowHost::window_retitled` moves the title bar and the taskbar
entry label from that one call, so the two cannot diverge.

Files, the terminal, and any future windowed app are decorated
this way with **no per-app decoration code** — the one place a served window is
dressed is the window manager.

Whether the frame is **resizable** is the opening app's own choice, carried on
its window `Create` as the `WindowSizing` it asks for
(`WindowRequest::Create { sizing, .. }` → `WindowClient::create`'s `sizing`
argument → `WindowHost::window_opened`). `WindowSizing::Resizable` gets a
live maximize/restore size toggle and the invisible resize edges above; the app
re-lays-out to each new client size the window manager reports
(`WindowEvent::Resized`), re-mapping its frame region with `WindowRequest::Resize`
so the resize keeps the window identity. A fixed-size app asks for
`WindowSizing::Fixed` — every client pixel reaches it and the size-toggle is
inert — so an app that renders at one size is never handed a size it did not
ask to handle. The two travel as one value, so "fixed" cannot arrive carrying
a minimum it would never be measured against.
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
  a `Create` does. A popup's frames are charged against the same
  per-client budget as a top-level window's
  (`tairix_window::client_frame_budget_bytes`), so "popup" cannot be used to
  pin more shared memory than `Create` may (`AGENTS.md` §5.4).
- **Undecorated by construction, not by a flag.**
  `ShellWindowHost::popup_opened` opens it through
  `DesktopShell::open_popup_window`, which adds the window to the
  compositor as its parent's transient but never calls `decorate_window`
  and never opens a taskbar entry — the same path the session's own trusted
  file picker already takes. No protocol "undecorated" bit exists to be
  forged.
- **Stacked directly above its parent, by the restack itself.** A popup is
  its parent's **transient**: `Compositor::add_transient_window` records
  the link and inserts the popup directly above its owner (and any
  transient already there), and every restack from then on moves the family
  as a unit, owner immediately below — `raise` and `lower` alike, whichever
  member is named. So nothing raised anywhere can land between a parent and
  its popup, and no caller re-asserts the arrangement per frame. That
  matters for more than tidiness: a re-assert *drops* the parent's retained
  frosted backdrop, and re-deriving an arrangement that already held cost a
  frosted, translucent terminal a full-window blur on every pointer sample
  its open menu saw. A family already at the end it is being moved to is
  left completely alone — no restack, no damage, not even an allocation.
  Raising an unrelated window puts it in front of *both*, as a desktop
  should: a window with a menu open is not pinned above its neighbours (nor
  above the taskbar) for as long as the menu lives.
- **A restack marks only where the family and the windows it crossed
  overlap.** Reordering two windows that do not overlap changes no pixel —
  nothing is drawn differently and no frost sees a different backdrop — so
  the move damages nothing. This is what makes opening a menu cheap in the
  arrangement that actually ships: the taskbar is a window above every
  application window, so an app is essentially never frontmost, and the
  raise that brings a family up crosses the bar. Marking the family's whole
  footprint instead threw away the owner's frosted backdrop every time,
  costing a 1000×700 translucent, blurred terminal a full-window blur
  (`blur_px 700000`) to open a 220×180 menu on it; it now costs the menu
  (`damaged_px 39600`, `blur_px 0`).
- **Parent death takes the popup with it.** Closing the parent — over the
  channel, from the frame's close control, or by the owning client dying
  (`client_exited`) — tears down every popup keyed to it; a `Close` naming
  the popup's own id tears down only the popup. The session drops the
  parent→popup link on either path, and `Compositor::remove` clears the
  transient link of anything the removed window owned, so no stale link can
  outlive a window on either side.
- **Presenting is unchanged.** `Present`, `SetBackdropBlur`, and `Close`
  act on a popup's id exactly as on a top-level id, and the popup's own
  events (pointer, key) arrive under its own window id with
  **popup-local** coordinates, so an app hit-tests its overlay against the
  popup's own viewport. One event mailbox serves both windows; the app
  demultiplexes on `WindowEvent::window_id`.

The first consumer is the graphical terminal, whose settings sheet is a popup
(`plans/GUI-TERMINAL.md` §9). Its window menu is not: a menu is the desktop's
one chain, opened through `OpenMenu` rather than drawn by the application
([Menus](menus.md)).

## Failing closed

Every fallible entry point returns a `Result`/`Option` rather than
panicking (`AGENTS.md` §2.9): `Compositor::new` and `Surface::new` return
`None` for a surface whose extent is unrepresentable or a stride too small
for one scanline, and an unsupported pixel format is refused at
construction rather than guessed (`AGENTS.md` §2.1). There is no `unsafe`
in the crate.

**An allocation the machine refuses is one of those refusals, not a
crash.** Every buffer here is sized by the screen or by a window: the
scan-out frame and back buffer are megabytes, a baked layer or a frosted
backdrop is a whole window's rectangle, and a decorated window's furniture
is rendered through an outer-sized transient. Userland's heap answers
exhaustion with a null pointer, which an infallible `Vec` growth turns into
a process abort — so each of these reserves its pixels through
`tairix_util::fallible` and hands the refusal back. That is what makes the
degradations documented above reachable rather than theoretical: a window
whose content cannot be reallocated keeps showing what it was showing, a
window whose furniture cannot be rendered draws its content over the
background band, a frost that cannot be captured retains nothing, and a
`Create` the session cannot back is declined with `Errno::OutOfMemory` —
the true reason, since the engine has already mapped the client's own frame
region of that same geometry, so the extent is proven representable and
only the allocator can still refuse.

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
(`plans/APPWIN.md` AW2): a versioned IPC protocol on the
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

Each request is framed to **its own** operation's length
(`WindowRequest::wire_len`), and a decode requires exactly that: a shorter
frame is truncation, a longer one is a field smuggled past the operation's
end, and both are refused. So the hot `Present` — one per composited frame
per window — sends the 36 bytes it carries rather than padding out to the
widest operation's block, and the bounded icon-bar declaration
(`SetAppBar`) can grow without the present path paying for it.
`WINDOW_MAX_REQUEST` is the endpoint's receive ceiling, not the shape of a
request — the client and the session each hold one buffer of it for the life
of the connection rather than taking one per call, so the ceiling's width
costs a present nothing. Events keep their one fixed `WindowEvent` frame.

A declaration's own length follows the menu it carries in two dimensions: one
fixed-width record per declared row, then the rows' text in one trailing
block. A menu holds every row's label, accelerator caption and disabled-row
reason in that one block rather than a widest-case buffer per row, so both
the frame and the model in memory cost what the rows actually say. The bounds
are format bounds a hostile client cannot widen: `APP_MENU_MAX_ROWS` (32)
rows per plate, `APP_MENU_MAX_TOTAL_ROWS` (64) across the whole menu,
`APP_MENU_MAX_DEPTH` (4) plates in a chain, and `APP_MENU_TEXT_BYTES` (1536)
of row text in total. Each row's text is taken from the block strictly in row
order and the block must be consumed exactly, so there is no offset to point
anywhere, no two rows can share bytes, and no text can ride along unread.

### Menus: the declaration, and the per-gesture open

Two operations carry a menu, and they differ in kind. `SetAppBar` is a
standing, **application**-scoped declaration the caller re-issues to replace
its whole icon-bar presence. `OpenMenu` is **per gesture** and window-scoped:
it asks the desktop to bring a chain up now, once, for a window the caller
owns (`plans/NEW-MENUS.md`). Both share one menu block, so a row record
cannot be laid out one way by a declaration and another by an open.

The **anchor** is window-local — physical pixels from the requesting window's
own client origin, exactly the space `WindowEvent::Pointer` reports in, so an
app anchoring a context menu at the press it just received passes back the
numbers it was given. It is never seat-global: an app is not told where its
window sits on screen, and never learns a pointer position inside a menu. It
is a *region* rather than a point, because that is what the placement rule
reads — a plate hangs clear of the control that opened it and flips to the
region's other side at a screen edge — and a zero extent is the point case,
so a context gesture and a menu-bar button share one rule. Any origin is
legitimate (the session clamps); only the far edge must be a representable
coordinate.

A **title** crosses the wire on an open and structurally cannot on a
declaration: the icon-bar menu is titled from the bundle's signed manifest,
so an application cannot title system chrome as something it is not, and a
titled declaration is refused at encode. An open's title is the app's own,
bounded and content-checked by the very validator a row label goes through.
An open carrying no rows is refused at both ends — there would be nothing to
open — where a declaration legitimately offers no menu at all.

The reply is only the **acceptance**, and it carries the session-minted,
never-reused **open id** (the shared status-plus-minted-id reply frame a
`Create` also begins with). The whole answer arrives later as exactly one
`WindowEvent::MenuClosed` naming that id: `Chosen(item)`, `Dismissed`, or
`Refused(reason)` over a closed reason vocabulary (`NoDisplay`, `SeatBusy`,
`NoResources`) whose unknown discriminant fails closed. It fits the existing
fixed `WindowEvent` frame, so an outcome costs every other event nothing.

The id is what makes the answer unmistakable. The engine keys the open to the
attested window owner, allows one unanswered open per window (a second is
`AlreadyExists`, which a well-behaved app cannot reach — its chain holds the
seat's grab, so the press that would open another is consumed there), and
requires an outcome to name that window's own unanswered open before it is
delivered, clearing it once the sink accepts. A second outcome for one open
is therefore refused rather than delivered, and an app that asked again while
a previous answer was still in its mailbox can tell the two apart instead of
reading one gesture's dismissal as the next one's. `WindowHost::menu_open_requested`
defaults to refusing, so a desktop that composes no menu service says so
rather than accepting a chain nothing will answer — and a refused menu is an
answer the app reports and carries on from, never a reason to draw one
itself.

### The one child that is not a plate: the information panel

A menu row is one of four kinds, and `Info` is the one whose child is a
surface rather than more rows: the session's own `FactList` of the owning
bundle's signed `AppInfo`, hanging where a submenu's plate would hang
(`plans/NEW-MENUS.md` §1.4). The app declares only that the row exists and
supplies none of the panel's text, so it cannot state an identity that is not
its own inside desktop chrome — and it states no accelerator, reason, mark or
emphasis, for the reason a submenu states none: it opens rather than acts. At
most one such row per menu, always at the top level, and it carries no id,
because there is no command for choosing it to answer with.

There is deliberately **no app-drawn child**. One was built — a `Panel` row
whose surface the application presented, detaching when its row was chosen —
and deleted for want of any client (`plans/NEW-MENUS.md` D19): a presentation
surface cannot conclude a gesture, and a chain the *desktop* opened for itself
has no application to ask in the first place. `CreatePopup` remains the way an
app opens a surface of its own beside a window.

### The service that answers it

The desktop session implements the host seam. `menu_open_requested` resolves
the window-local anchor against the owner's live client origin, builds the
chain's model from the wire menu, and brings the chain up. It reads no file and
waits on no client: the information row's attested facts come from the identity
the icon-bar service has already resolved, and a process with none gets no
information row rather than a fabricated panel.

The chain itself — placement, bands and their drag, arrival-driven children,
the grab, traversal, dismissal and lifetime — is
`userland/gui/session/src/menu.rs`, and is described in
[Menus](./menus.md). It touches no compositor: the session presents the
surfaces it lists and takes down what it no longer has, so a plate cannot
outlive the state that placed it.

The same service answers the desktop's *own* menus, which hand it a model built
in process rather than one decoded from the wire; both resolve through one seat
rule, so a menu cannot take the grab from the lock screen or the trusted picker
by arriving from either direction. Nothing on this channel says a plate has been
*drawn* — the reply is only the acceptance — so the session announces that
itself, once per open, after the frame carrying the chain reached the display.

The renderer, the chain's placement, the grab and the dismissal rules are the
session's menu service; the contract above is what an app sees of them.

An app also sets its own window's **backdrop-blur** radius over the
channel: `WindowClient::set_backdrop_blur(window_id, radius_px)` sends
`WindowRequest::SetBackdropBlur`, whose decode refuses a radius above
`WINDOW_BACKDROP_BLUR_MAX_PX` and a frame longer than the operation needs.
The engine
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
  (`ipc_call`) and `EventSource` (the app's own event endpoint) seams. The
  client remembers each window's last presented frame index and extent, so
  `WindowEvents::wait` answers a `RedrawRequested` by re-presenting that
  frame with full-window damage before returning the event to the app —
  no app has to implement the handshake, and an app that wants to render
  genuinely fresh pixels still sees the event. `EventSource` states the
  drain (`try_next`, never waits) and the park (`park`) separately, with
  `next` defaulted as drain-then-park; an app with work of its own —
  decoding a folder's icon artwork, rendering a gallery of wallpapers —
  drains with `WindowEvents::try_wait` so input is served ahead of that
  work and the loop parks only when neither has anything left. Both paths
  decode and answer the redraw through one definition.

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
(`userland/apps/files`), autostarted by the session and
proven end to end by the autoload QEMU vertical's
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
