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

1. Each window owns a `Surface`: a dense, row-major buffer of
   **premultiplied-alpha** `Pixel`s.
2. `Compositor::composite` walks the damaged screen regions and, for
   every dirty pixel, blends each covering window *over* the opaque
   background, bottom-to-top in z-order, using the Porter–Duff *over*
   operator (`Pixel::over`).
3. Each composited pixel is encoded into a byte frame laid out for the
   active `DisplayMode` (`Rgba8888` or `Bgra8888`).
4. `Compositor::present` hands that frame to a `Display` driver.

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

## Hardware acceleration

When the display driver exposes the optional
`AcceleratedDisplay` seam (`AGENTS.md` §10),
`Compositor::present_accelerated` lets the hardware composite the scene
instead of the CPU. It encodes the scene back-to-front as one solid
background layer, one `AccelLayer` per visible window (its surface baked
with that window's opacity and rounded-corner coverage through the same
`sample_local` path the software compositor uses, so the hardware result
matches pixel-for-pixel), and the cursor on top, then hands the stack to
`AcceleratedDisplay::present_layers`.

The software path is always the fallback: if the scene exceeds the
engine's reported `AccelCaps` — more layers than it has planes, or a
layer larger than it can source — the compositor composites the whole
frame in software and presents it instead, so a hardware frame is never
partial (`AGENTS.md` §2.9). The first driver to implement the seam is the
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
`Compositor::composite` recomputes only those pixels and then clears the
damage, so an idle desktop costs nothing to recomposite.

## Server-side window decorations

Window decorations — a title bar with the four command controls (close,
minimize, put-to-back, size-toggle), an active/inactive frame rim, and a
corner resize grabber — are drawn by the **window manager**, never by an
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
- **Rendering.** Each decorated window keeps a pre-rendered, outer-sized
  decoration `Surface` painted through `WindowFrame::render` /
  `TitleBar::render` (rim, body, the sanitised title text via `lib/font`,
  and the four command controls) plus a corner `ResizeGrabber`, using the
  one `lib/raster` fill and the shared rounded-corner path — so the rim's
  rounded corners stay transparent and the desktop shows through. The
  compositor samples that decoration in the reserved band and the client
  content inside it (`Window::sample_local`), for both the software and the
  hardware-accelerated present paths. The furniture is animation-free, so it
  is reduced-motion correct by construction, and high contrast thickens the
  command-glyph and grip strokes.
- **Activation and title.** `Compositor::set_active_frame` repaints a
  window's rim, title, and controls for the focused/unfocused state the
  `InputRouter` tracks; `Compositor::set_window_title` repaints the title
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

## Decorated windows in the live session

The desktop session (`userland/gui/session`) turns decorations on for every
**served application window**. When a window opens over the channel,
`ShellWindowHost::window_opened` opens the bare window through the shell and
then calls `DesktopShell::decorate_window`, which attaches a `WindowFrame`
(movable, presenting a fixed size — the default apps render at one size, so no
resize grabber is drawn and the size-toggle reports no change) and labels its
title bar with the channel's `WindowTitle`. Files, the terminal, and any future
windowed app are decorated this way with **no per-app decoration code** — the
one place a served window is dressed is the window manager.

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
desktop `KeyInput` codec), window-local pointer events, and
`CloseRequested` (the app owns the close decision) — delivered to that
endpoint, where the app **parks** until one arrives; it never polls.

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
  (`ipc_call`) and `EventSource` (parked endpoint wait) seams.

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
(`userland/apps/files`), spawned from the start menu and proven end to
end by the autoload QEMU vertical's click-through (three verified
screendumps: desktop, served window, re-themed desktop); the terminal
landed with AW4.

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
channel-order encoding, the `Display` present seam, the accelerated
layer-encoding present path (background + window layers, hidden-window
omission, and the over-budget / over-size software fallbacks), and input
routing (hit-testing, click-to-activate focus and raise,
desktop-clears-focus, move-grab drag, and the fail-closed grab edge
cases).
