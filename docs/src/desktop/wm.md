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
   operator (`Pixel::over`). It returns the `DamageRegion` it actually
   recomposited (screen-clipped), which is what the present step moves.
3. Each composited pixel is encoded into a byte frame laid out for the
   active `DisplayMode` (`Rgba8888` or `Bgra8888`).
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
the frosting is missing from. The first driver to implement the seam is the
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

The blur itself is a **separable box blur** (`blur::box_blur`): a
horizontal pass then a vertical one, each carrying a running sum so the
window slides by one add and one subtract per output. The cost is
proportional to the rectangle's *area* whatever the radius, never to area
× radius. Both scratch buffers belong to the compositor and grow to the
largest frosted rectangle the session has needed, so a frosted window
allocates nothing after its first frame.

Every channel is averaged, alpha included: on premultiplied data that is
the same convex combination of the contributing colours that compositing
them would give, so the `colour <= alpha` invariant survives and no halo
appears at a translucent edge. Samples past an edge replicate that edge,
which confines the effect to the window's own rectangle — it can neither
pull a neighbour's pixels in nor write outside its bounds — and keeps the
divisor constant, so a uniform backdrop comes out exactly unchanged.

The mix back into the back buffer is weighted by the window's own
rounded-corner coverage (the single `Window::row_rounding` shape
definition its *pixels* are weighted by), so a rounded window's frosting
fades out across exactly the arc its own pixels fade out across and no
square edge shows outside it.

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

`DamageRegion` records the screen rectangles that changed since the last
frame (a window was added, moved, restyled, hidden, raised, or removed).
`Compositor::composite` recomputes only those pixels, returns the region
it recomposited, and clears the damage, so an idle desktop costs nothing
to recomposite.

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

## Server-side window decorations

Window decorations — a title bar with the four command controls (close,
minimize, put-to-back, size-toggle), the frame rim, and a corner resize
grabber — are drawn by the **window manager**, never by an
app (`AGENTS.md` §10, `plans/GUI-CONTROLS-DESIGN.md` §1, §11.17–§11.23;
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
  furniture. `clear_window_frame` collapses the band back to the bare
  surface. A DPI (`set_scale`) or theme (`set_theme`) change re-resolves the
  band for every decorated window.
- **Rendering.** A decorated window's furniture is a `WindowChrome`: the
  four strips the frame actually draws into — the top (title) band, the
  bottom band, and the two side borders — painted through
  `WindowFrame::render` / `TitleBar::render` (rim, body, the sanitised title
  text via `lib/font`, and the four command controls) plus a corner
  `ResizeGrabber`, using the one `lib/raster` fill and the shared
  rounded-corner path — so the rim's rounded corners stay transparent and
  the desktop shows through. Only the strips are kept: the region between
  them is never sampled (the compositor draws the window's own content
  there), so retained bytes follow the band thickness and not the window
  area — a 1920×1080 window's furniture costs roughly a seventeenth of what
  one outer-sized surface did. The compositor samples those strips in the
  reserved band and the client content inside them (`Window::row` /
  `Window::sample_local`), for both the software and the
  hardware-accelerated present paths; a screen row needs at most two of
  them, since a row is either in the title/bottom band or crosses the client
  between the two side borders. The furniture is animation-free, so it is
  reduced-motion correct by construction, and high contrast thickens the
  command-glyph and grip strokes.
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
  carries. Both confine their damage to the furniture bands — a focus flip
  or title edit never recomposites the client (`AGENTS.md` §2.16).

- **Furniture hit map.** `Compositor::frame_hit` classifies a screen point
  against a decorated window's `WindowFrame::hit`, returning a typed
  `FurniturePart` (title bar, a command control, a resize edge, the inert
  rim, or the client). The `InputRouter` consults it *before* the client and
  before the root-viewport scrollbar hit map, so a press on the frame is never
  reported to the app as `Activated` and an app look-alike inside the client
  can never impersonate a real frame control (`plans/GUI-CONTROLS-DESIGN.md`
  §1, §11.17–§11.18). A non-resizable window classifies its border as inert
  `Frame`, never a resize edge, so a fixed-size window cannot be dragged
  larger. The client-press position the app receives is reported relative to
  the inset **client** rectangle, so decorating a window never shifts its
  content coordinates.
- **Pointer and keyboard routing.** A title-bar press begins the cooperative
  move-grab; a command-control press captures the frame, feeds the click to
  `TitleBar::on_pointer`, and emits `WindowControl { window, control }` on the
  completed release; a resize-edge press (resizable windows only) drives the
  shared `ResizeGrabber`. When the frame furniture holds the keyboard, arrows
  move focus between the controls and Space/Enter activate one
  (`Compositor::frame_key` → `TitleBar::on_key`); a client press returns the
  keyboard to the app.
- **Typed lifecycle (no new syscall).** Each command control maps to a window
  lifecycle action in one shared place
  (`tairix_desktop_session::window_control_event`), so the live serve loop and
  the tests drive the same rule: **Close** returns
  `WindowEvent::CloseRequested` (the app tears down cooperatively — the window
  manager never destroys a window behind the app's back); **Minimize** hides
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
`WindowTitle`. Files, the terminal, and any future windowed app are decorated
this way with **no per-app decoration code** — the one place a served window is
dressed is the window manager.

Whether the frame is **resizable** is the opening app's own choice, carried on
its window `Create` (`WindowRequest::Create { resizable, .. }` → `WindowClient::create`'s
`resizable` argument → `WindowHost::window_opened`). A resizable window is drawn
with a resize grabber and a live maximize/restore size toggle; the app
re-lays-out to each new client size the window manager reports
(`WindowEvent::Resized`), re-mapping its frame region with `WindowRequest::Resize`
so the resize keeps the window identity. A fixed-size app passes `resizable:
false` — no grabber is drawn and the size-toggle is inert — so an app that
renders at one size is never handed a size it did not ask to handle. The file
**viewer** (`userland/apps/viewer`) is the shipping resizable app: it re-wraps
its text to the new width, preserves the reader's scroll position across the
resize, and fails closed (keeping the current surface) if a new frame region
cannot be allocated or the session refuses the re-map. Files and the terminal
present fixed-size windows.

`DesktopShell::sync_active_frame` keeps exactly one window showing its active
frame: on every focus change — a click-to-activate press, a taskbar activation,
an open, a close, a minimize — the newly focused decorated window is activated
and the previously active one reverts to inactive. It is a no-op for an
undecorated focus, so the session's own **trusted file picker** — session
chrome dismissed by its own keys, not an app the window manager dresses — opens
undecorated and never gains an inert title bar, while still correctly
deactivating whatever app window it drew focus away from.

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
each damaging exactly its footprint), the accelerated
layer-encoding present path (background + desktop + window layers,
hidden-window omission, and the over-budget / over-size / backdrop-blur
software fallbacks), the backdrop blur (the box blur's own identities — a
uniform field unchanged, an impulse spread symmetrically, radius 0 and a
one-pixel region identities — plus the composited effect: a spread
backdrop, a no-op at radius 0, confinement to the window rectangle, the
logical radius following the output scale, rounded corners left alone, and
a change behind a frosted window repainting it to exactly the pixels a
whole-screen composite gives), and input routing (hit-testing,
click-to-activate focus and raise, desktop-clears-focus,
`DesktopPointerMoved` carrying no position of its own and `DesktopKey`
reporting focus-on-desktop, move-grab drag, and the fail-closed grab edge
cases).
