# COMPOSITOR-WORK.md — Window decorations in the compositor (server-side furniture)

This is the staged build plan for giving every windowed app real window
decorations — a title bar with Close / Minimize / PutToBack / SizeToggle
controls, the frame rim, and invisible resize edges — by rendering
them in the **window manager**, not in any app. It is **binding under
`AGENTS.md`** — read `AGENTS.md`, `PLAN.md` Stage 7, `plans/GUI-CONTROLS-DESIGN.md`
(the control/furniture design this consumes), `plans/APPWIN.md` (the window
channel + WM-presented windows this builds on), and `plans/DISPLAY.md` (the
seat/display model beneath it) first; every rule in all of them applies here
without exception.

## 0. Why this work exists (findings, binding for this plan)

- **Decorations are the window manager's job, not the app's.** The design is
  explicit and server-side: the WM owns outer window-frame and furniture
  rendering, hit testing, pointer capture, move/resize behaviour, stacking
  actions, minimization, and size-state transitions. Applications provide
  typed metadata and receive typed events through the existing window path;
  they never paint over or intercept window-manager chrome, and the WM keeps a
  **separate furniture hit map** so an app can never impersonate a real frame
  control (`plans/GUI-CONTROLS-DESIGN.md` §1, §11.17/§11.18, §424, §1189).
  The Files app is therefore **correct as-is** — it draws only its browse
  content. It has no decorations because nothing draws them yet.
- **The furniture family already exists, unconsumed.** `lib/controls::window`
  provides the complete, tested family — `WindowFrame`, `TitleBar`,
  `WindowControl` (kinds Close/Minimize/PutToBack/SizeToggle), `ResizeGrabber`,
  `ScrollCorner` — with `layout`/`render`/`hit`/`on_pointer`/`on_key` and typed
  events (`WindowControlAction`, `TitleBarEvent`, `ResizeEvent`,
  `FurniturePart`). It is a *reference composition*: no WM consumes it.
- **The WM composites content only.** `userland/gui/wm` blits per-window
  content `Surface`s with rounded corners (`corner.rs`), damage tracking
  (`damage.rs`), a cursor overlay (`cursor.rs`), click-to-activate + move-grab
  (`input.rs`), and the **root-viewport scrollbar** furniture (`viewport.rs`:
  `RootViewport`, `FurnitureLayout`, `FurnitureHit`, `hit_test`). There is **no**
  `WindowFrame`/`TitleBar` composition. `userland/gui/wm/Cargo.toml` does not
  depend on `tairix-controls`.
- **The title already flows over the channel.** `lib/window` carries
  `WindowTitle` on `Create`, and `lib/abi::window_ipc::WindowEvent` already has
  `CloseRequested { window_id }`. Today the title is consumed only as the
  taskbar label; nothing renders it as a title bar.

The `viewport.rs` root-viewport scrollbar is the exact precedent to follow:
reserve a stable furniture gutter, shrink the client rect, keep a furniture hit
map so a furniture press is never `FurnitureHit::Client`, and route furniture
input to typed actions. This plan extends that pattern from the inner scrollbar
to the outer frame.

## 1. Guiding rules (do not violate)

- **Nothing here is deferred, stubbed, no-opped, or "for now."** Every stage
  lands *complete* (`AGENTS.md` §2.19, §27): a "title bar today, resize later"
  split is exactly the deferral the charter forbids. If a stage genuinely
  depends on prerequisite work, that prerequisite is part of the same change or
  the conflict is raised with the User (§15.7).
- **Wire decoration *rendering and input* once, in the WM; no app draws its own
  chrome.** Because decorations are server-side, every app (Files, Terminal,
  Viewer, and any future Switchboard) gets decorated by the WM composing the
  furniture around each window. Adding a per-app decoration path, or letting an
  app draw its own title bar, is a design violation
  (`plans/GUI-CONTROLS-DESIGN.md` §1, §424). This is a rule about *chrome*, not
  about the window channel: an app crate **may** be changed to *react* to a
  typed lifecycle event the WM delivers over the existing window path (a close
  request it already honours, a minimize notice, a new client size on a
  resize/maximize) — that is cooperative lifecycle handling, not a decoration
  path. Such app-crate changes are permitted provided they are first-class,
  correct, and well-reasoned; the ban is only on an app painting or
  intercepting window-manager furniture.
- **No second visual recipe or constant (§2.2).** Frame/title metrics, palette
  roles, and motion come from `lib/theme`; drawing goes through the one
  `lib/raster` path already used for rounded corners; the rounded-corner math
  is the existing `corner.rs`/`round_rect_coverage` path — no new recipe. The
  furniture geometry is `lib/controls::window`'s `layout`/`hit`, not a
  reimplementation in the WM.
- **No new syscall, no ambient authority (§4, §5.4).** Cooperative
  close/minimize/put-to-back/size-toggle ride the **existing** window path as
  typed `WindowEvent`s; the WM validates the event targets a live window owned
  by the addressed client. Force-quit stays the separate capability-checked
  recovery path — it is not a title-bar button.
- **Headless stays first-class (§17.3).** `userland/gui/wm` is a
  `userland/gui/*` crate; the one-way dependency edge holds and a headless
  build simply excludes it. No non-GUI crate gains a `tairix-controls`/GUI edge.
- **Client can never reach the furniture (§424, §1189).** The client content
  surface never overlaps, clips, or receives input from the frame, title bar,
  controls, or grabber; a furniture press is furniture in the hit map, never
  `FurnitureHit::Client`.

## 2. Stages

**Status:** Stages A–H are **done**. Server-side window decorations are live:
every served application window is decorated by the window manager, client-driven
resizability is live (the file viewer opens resizable and re-lays-out on
`Resized`), and the whole-project validation gate is green.
Per the User's direction, one full stage lands per change.

Each stage lands complete — its rendering for **both** dark and light themes,
reduced-motion and high-contrast behaviour, its pointer/keyboard/focus paths,
and its `#[cfg(test)]` tests — before the next begins.

### Stage A — WM depends on `lib/controls`; frame layout + reserved client rect — DONE

The window-manager geometry foundation for decorated windows is complete and
tree-green. What it now guarantees:

- `userland/gui/wm/Cargo.toml` depends on `tairix-controls`; the crate root
  re-exports the furniture family it will compose.
- `WindowFrame` exposes the single outer↔client derivation both directions:
  `FrameInsets`, `WindowFrame::insets()`, and `WindowFrame::outer_for_client()`
  (the inverse of `layout`). `layout` and `insets` share one `edges()` metric
  helper, so the frame band has exactly one definition (no §2.2 duplication).
- `Window` holds an opt-in `Option<WindowFrame>` (mirroring the existing
  `Option<RootViewport>` precedent). `bounds()` returns the outer rectangle for
  a decorated window; `client_rect()`/`frame()` expose the inset client and the
  frame; content sampling maps outer-local→content coordinates so the reserved
  band shows the background and the client never overlaps furniture. The
  undecorated path is byte-identical.
- `Compositor` owns the active `Theme` (`theme()`/`set_theme()`), offers
  `set_window_frame`/`clear_window_frame`/`window_frame`/`window_client_rect`,
  and re-resolves every window's band on scale or theme change.
- Tests cover insets/`outer_for_client` round-trips at reference and scaled DPI
  under both themes, plus WM outer-band reservation, client-vs-background on
  composite, clear-reverts, rescale-grows-band, theme-switch, and
  undecorated-unchanged.

No furniture is rendered or hit-tested yet, no ABI was touched, and no window
opts into a frame in the running desktop — so there is no behavioural or visual
change yet. That is Stage B onward.

### Stage B — Compose and render the furniture — DONE

The furniture chrome is rendered around every decorated window. What it now
guarantees:

- A decorated window's furniture is a `WindowChrome` (`wm/src/chrome.rs`): the
  four strips the frame actually draws into — the top (title) band, the bottom
  band, and the two side borders — rendered by `Window::render_chrome` through
  `WindowFrame::render`/`TitleBar::render` (rim, body, the sanitised title via
  `lib/font`, the four `WindowControl` buttons), using the one `lib/raster`
  fill and the shared rounded-corner path (no second recipe). The rim's
  rounded corners stay transparent so the desktop shows
  through. The strips are cut from one transient outer-sized render because
  the drawing primitives refuse a negative-origin destination; only the strips
  are kept, so retained bytes scale with the band and never with the window
  area. A zero-extent edge holds no surface at all.
- **The window's silhouette is the frame's rim, and the client is clipped to
  it.** `Window::shape` reports one shape for either kind of window — a
  decorated window's `WindowFrame::rim` radius over its outer rectangle, a plain
  window's own corner style — and both its pixels and its frosted backdrop are
  weighted by it. The client, whose rows are square, is cut to the plate the
  frame fills inside that rim (`FrameRim::plate`); a pixel the plate does not
  fully cover is the frame's, so content can neither cover the rim nor square
  off the corner. The top and bottom strips are therefore as deep as the radius
  wherever the reserved inset is thinner, and the side strips take only the rows
  between them: a corner row is furniture over its whole width, so a window with
  no content still draws its curve. `TitleBar::render` lays no ground of its own
  — the frame's plate is already under it, rounded.
- **The chrome is not stored in the window.** The `Compositor` owns one
  `ReclaimCache<WindowId, WindowChrome, ChromeEpoch>` (`chrome_cache`, built on
  `tairix_reclaim::screenful_ui_cache`, ceilinged at one screenful), so the
  desktop's total furniture is bounded, charged to the seat, wiped on release
  (it carries window titles) and given back the moment the kernel reports
  memory pressure. The epoch is `(scale percent, theme generation)` — a
  generation counter, not `ThemeId`, because a contrast/motion variant keeps
  its id. A single window's change (title, focus, resize, size-state, frame
  attach/detach, removal) is a per-key `invalidate` through the one
  `Compositor::mutate_frame` helper every such mutation runs through; only a
  scale or theme change drops the whole cache.
- The cache is an accelerator, never a correctness requirement: each pass
  (`composite`, `present_accelerated`) first runs `ensure_chrome` under the
  exclusive borrow, then reads with `peek` during the immutable row/column
  walk, and anything the cache refuses or evicts mid-pass is built for that
  pass alone. The composited frame is byte-identical warm, emptied, and with a
  zero budget — asserted.
- `Window::row`/`sample_local` take that chrome and sample it in the reserved
  band with the inset client content inside it, so both the software composite
  and the hardware-accelerated `encode_layers` path draw the furniture
  identically; the client never overlaps the band. A screen row needs at most
  two furniture spans (a title/bottom row takes one strip; a row crossing the
  client takes the left and right borders), which is what `WindowRow` carries.
- The session builds the cache alongside the cursor and icon caches from the
  same seat, output byte size, `tairix_rt::pressure::gauge()` and log sink, and
  trims (`DesktopShell::trim_caches`) and tears it down (`teardown`) on the
  same paths.
- The title the WM receives on the channel (`WindowTitle`) is rendered in the
  title bar via `Compositor::set_window_title`, not merely used as the taskbar
  label. It elides with the shared `ELLIPSIS` mark (`BitmapFont::elide_to_width`)
  rather than being cut, because a title may be a path.
- A window command wears no perimeter of its own in any state and no plate at
  all while it rests: it is bar-seated (`FrameColors::face`), so hover and
  press are the shared plate wash and an accent edge never reads as a line
  drawn round the window's corner.
- The four commands sit in two corner clusters — put-to-back then close at the
  leading edge, minimize then size-toggle at the trailing one — and the
  **owning application's identity icon** leads the title text in one group
  left-justified in the span between them, one gap past the leading cluster; a
  window with no identity reserves no slot and its title takes that leading
  edge alone. The slot is `crate::paint::icon_slot_side` and the artwork is
  drawn by the shared `paint_icon_slot`, so there is no second icon path. The
  artwork is desaturated by activation as it lands — nearly all its colour on
  the active frame, none on an inactive one — through the one saturation
  definition in `lib/raster`, so one cached full-colour icon serves both. The
  icon is inert — part of the draggable region, never a control.
  `Compositor::window_title_icon_side` reports the side to rasterise at and
  `Compositor::set_window_identity` takes the identity plus that artwork,
  dirtying only the title band and dropping only that window's chrome entry.
  Identity comes from the caller `WindowServer` attested:
  `WindowHost::window_opened` carries the `ProcId`, `ShellWindowHost` records
  it against the window, and `resolve_window_identities` drains those records
  immediately after the serve pass (the attested-caller table and the launch
  records are both borrowed while a request is served), mapping pid →
  `LaunchTable` bundle → that bundle's own `AppInfo` icon through the one
  `ArtworkCache` a taskbar pin uses. That same resolution also gives the
  window's taskbar entry its icon (`Taskbar::task_icon_side` →
  `TaskList::set_artwork`), from one manifest read, so the bar shows the
  application that owns the window whether or not it is pinned and the two
  surfaces cannot disagree. No app-supplied string can choose it, an
  unidentified caller gets no icon, an unresolvable one gets the built-in
  `IconKind::AppBundle` glyph — a window always opens — and a second window of
  the same application is a cache hit per slot, not a second read and decode.
- Activation follows the focused window through
  `Compositor::set_active_frame`, which repaints the title and controls;
  attention requests are preserved rather than clobbered by a focus change.
  The rim itself does **not** track focus: every window wears the one quiet
  `frame` neutral at every activation, because the rim is the line the eye
  reads a window's shape by — brightening it on focus made the boundary the
  loudest mark on the desktop and left every other window reading as switched
  off. The title bar carries focus in its text tone, joined under heavy
  contrast by a doubled inner rim line so the distinction is a difference in
  shape too (spec §11.17).
- A focus change or title edit repaints only the furniture bands
  (`Window::furniture_bands`/`title_band`), never the client — damage stays
  confined to the furniture.
- Tests cover dark and light theme render, the one quiet rim tone at either
  activation (with the two frames still differing, since the title bar shows
  focus), the title being drawn, reduced-motion pixel-identical render,
  high-contrast glyph thickening, and furniture-confined damage on a focus
  flip and a title edit.

No furniture is hit-tested or wired to a lifecycle action yet, and no ABI was
touched — that is Stage C onward.

### Stage C — Furniture hit map + pointer/keyboard routing — DONE

The window manager classifies and routes every frame-furniture interaction to
typed outcomes, entirely inside `userland/gui/wm`. What it now guarantees:

- `Compositor::frame_hit` classifies a screen point against a decorated
  window's `WindowFrame::hit` (→ `FurniturePart`); `input.rs` `press_primary`
  consults it first (then the root-viewport `hit_test`), so frame furniture and
  scrollbar furniture share one press-classification step and a frame press is
  never `Activated`/delivered to the client.
- A title-bar press begins the existing move-grab (`begin_move` → `Moved`/
  `MoveEnded`); a resize-edge press begins a resize-grab that drives the shared
  `ResizeGrabber` (`ResizeEvent`), recomputes the clamped outer rectangle per
  edge (minimum-client floor `MIN_CLIENT_W`/`H`), and applies it through
  `Compositor::resize_window` (client geometry, origin, and decoration
  following), reporting `Resized`/`ResizeEnded`; Escape cancels and restores
  the pre-drag geometry exactly.
- **The frame is the window manager's; the pixels are the client's.** Neither
  `Compositor::resize_window` nor `resize_window_client` touches the client's
  content buffer: they move the geometry the compositor draws and lays
  furniture out from. The buffer is sized by the frame the *client* presents
  (`Compositor::present_window_content` establishes it when the one held
  describes a different geometry), and the compositor draws the part of it
  that lands inside the client area. This is what makes the live drag correct:
  the frame moves on every motion while the app is told its new size once, at
  `ResizeEnded`, so in between the app is still presenting the geometry it
  last knew — reshaping its buffer under it would refuse every one of those
  presents, which an app cannot distinguish from a dead session (it exits).
  It also costs no per-motion copy of the window's pixels.
- A command-control press captures the frame (`control_grab`), feeds the click
  to `TitleBar::on_pointer`, and emits `InputResponse::WindowControl { window,
  control }` on the completed release. Keyboard control activation routes
  through `Compositor::frame_key` → `TitleBar::on_key` (arrows move focus,
  Space/Enter activate) when the frame furniture holds the keyboard; a control
  press claims that focus and a client press returns it, so a decorated
  window's content keeps its keys until the user reaches for the furniture.
- **A furniture control shows its border only while pressed or being navigated
  with the keyboard, and returns to rest once its command fires.** A completed
  activation (`WindowControl::on_pointer`/`on_key`) clears the control's
  hover/press highlight and keyboard focus ring (`WindowControl::rest`), so no
  border lingers after the click — a genuine hover is re-established by the next
  pointer move — and a maximize/put-to-back that relocates or hides the button
  leaves no stale highlight behind, as a desktop title-bar control does.
- The furniture press/keyboard repaint marks only the furniture bands dirty
  (never the client), and the resize corner is reserved clear of the scrollbar
  tracks/thumbs (`plans/GUI-CONTROLS-DESIGN.md` §1218) — asserted.
- Tests cover each furniture region hit-test and its exclusion from the client,
  title-bar drag→move, corner resize grow, resize clamp-to-minimum + Escape
  restore, pointer and keyboard control activation, client-press keyboard
  return, and resize corner ∩ scrollbar track = ∅.

No served window opts into a frame in the running desktop yet, so — as with
Stages A–B — there is no behavioural change in the live session; wiring the
typed outcomes to the window lifecycle over the channel is Stage D, and turning
decorations on is Stage E.

### Stage D — Typed control actions → window lifecycle — DONE

Every title-bar command control now maps to a real window-lifecycle action,
wired through the existing window path with no new privileged syscall. What it
guarantees:

- **One shared mapping.** `tairix_desktop_session::window_control_event` is the
  single place the four `WindowControlKind`s become lifecycle, so the live serve
  loop (`run.rs`) and the host tests drive the same rule:
  - **Close** → returns `WindowEvent::CloseRequested { window_id }`; the WM never
    destroys the window behind the app's back — the app tears down cooperatively.
    Ownership/liveness is enforced by the engine's `deliver_event`, which routes
    only to the window's own registered endpoint (its attested owner).
  - **Minimize** → `DesktopShell::minimize_window` hides the window and marks its
    taskbar entry minimised (`TaskList::minimise` / `TaskBridge::minimize`) and
    drops focus; returns `WindowEvent::Minimized { window_id }` so the app may
    pause non-essential work.
  - **PutToBack** → `Compositor::lower` restacks to the bottom of the z-order — a
    window-manager-local action with no app-ward event.
  - **SizeToggle** → `Compositor::toggle_window_size` maximizes to the session
    **work area** (screen minus the taskbar band, `DesktopShell::work_area`) or
    restores the pre-maximize geometry, flips the frame furniture size state, and
    returns `WindowEvent::Resized { window_id, width_px, height_px }` carrying the
    new client size. A window that cannot maximize (undecorated or non-resizable)
    yields nothing.
- **The resize protocol is complete.** The ABI gained `WindowEvent::Minimized`,
  `WindowEvent::Resized`, and `WindowRequest::Resize` (a resizable app re-maps its
  frame region at the new size, keeping the window id/owner/endpoint/taskbar
  entry); the engine's `WindowHost::window_resized` moves the compositor's
  client geometry to the size the app re-mapped (`resize_window_client`), and
  the app's next present sizes its buffer. An interactive resize-grab
  (`ResizeEnded`) forwards the settled client size to the app the same way,
  once, at the end of the drag.
- **Force-quit** is **not** a title-bar control — it remains the separate
  capability-checked recovery path.
- **Resizability is per-window and opt-in.** The mechanism (grabber,
  size-toggle, resize protocol) is per-app: an app that renders at one size is
  offered neither affordance and never receives a `Resized`, and treats
  `Minimized`/`Resized` as honest no-ops. A resizable app handles `Resized` by
  re-mapping its region via `WindowClient::resize` (Stage F).
- **Tests** cover: Close yields `CloseRequested` for the owning window and
  nothing for a non-served window; a resize/close/present against a foreign or
  dead window is refused fail-closed (`lib/window`); minimize hides the window +
  marks the taskbar entry + emits `Minimized`; put-to-back restacks with no
  event; size-toggle maximizes to the work area then restores and emits
  `Resized`; the engine `Resize` re-maps the region and the host moves the
  window's client geometry; and a resize-grab leaves the client's own pixels
  untouched while a present at a new geometry establishes their buffer.

### Stage E — Decorations live, documented, gated — DONE

Decorations are turned on in the running desktop, and the whole feature is
complete. What it now guarantees:

- **Served application windows are decorated; the picker is not.**
  `DesktopShell::decorate_window` attaches a movable, fixed-size `WindowFrame`
  (no resize grabber; the size-toggle is disabled and inert) and labels its
  title bar with the channel `WindowTitle`. `ShellWindowHost::window_opened`
  calls it for every served window, so Files, the terminal, and any future
  windowed app are decorated with **no per-app decoration code**. The session's
  own trusted file picker is session chrome, dismissed by its own keys, so it
  opens *undecorated* — no inert title bar.
- **An app retitles its own window.** `WindowRequest::SetTitle` (`OP_SET_TITLE`
  12) carries a `WindowTitle` for a window the caller owns; the server applies
  the same ownership check `Present`/`Resize` use before touching any state and
  answers a foreign id `NotFound`. `ShellWindowHost::window_retitled` moves the
  title bar and the taskbar entry label from one call
  (`DesktopShell::retitle_window` → `TaskBridge::retitle`), so the two cannot
  diverge; a session-owned undecorated window relabels on the bar alone. The
  file picker uses it to show where it is browsing, spelled by the shared
  `tairix_browse::vfs::spell_title_location` against a budget derived once from
  `WINDOW_TITLE_MAX` minus its fixed prefix.
- **A secondary press on Close is its own gesture.** `WindowControl::on_pointer`
  resolves it to `WindowControlAction::AlternateInvoked` *without* touching the
  control's press latch or arming it, so the drawn state is provably
  unchanged and the control never also activates; the window manager reports
  `InputResponse::WindowControlAlternate` and the session maps it to
  `WindowEvent::AlternateCloseRequested` (`EV_ALTERNATE_CLOSE_REQUESTED` 12)
  for the owning app only. It closes nothing. A session-owned window has no
  channel id, so the press is dropped rather than leaked.
- **Exactly one active frame follows focus.** `DesktopShell::sync_active_frame`
  reconciles the compositor's active-frame decoration with the window manager's
  focused window on every focus change (click-to-activate, taskbar activation,
  open, close, minimize). It is a no-op for an undecorated focus, so focusing
  the picker still correctly deactivates the app window it drew focus from.
- **The controls drive the lifecycle end to end.** A click on a real title-bar
  control routes through the input router to `InputResponse::WindowControl` and
  is mapped by the one shared `window_control_event` to Close→`CloseRequested`,
  Minimize→hide+taskbar-minimised+`Minimized`, PutToBack→restack (no event),
  SizeToggle→`Resized` (nothing for a fixed-size window). No new syscall, no
  ambient authority; the app tears itself down on close.
- **Docs.** `docs/src/desktop/wm.md` documents furniture ownership, the hit
  map, pointer/keyboard routing, the typed lifecycle, and the live-session
  decoration (served apps decorated, picker not).
- **Tests.** `userland/gui/wm` covers rendering/hit-map/routing/resize;
  `userland/gui/session` covers decorate-on-open (both the shell `open`+
  `decorate` path and the real `window_opened` serve path), the active frame
  following focus across open/click/close/minimize, and an end-to-end vertical
  clicking every command control and mapping it through the lifecycle; the AW3
  click-through vertical asserts the presented window is decorated.

### Stage F — Client-driven resizability, live and per-app opt-in — DONE

An app can now ask to be resizable, and the file viewer does, end to end. What
it guarantees:

- **The `resizable` request rides the existing create, no new syscall.**
  `WindowRequest::Create` carries a validated `resizable` flag (the reserved
  byte after the title; decode refuses anything but `0`/`1`), threaded through
  `WindowClient::create` → the engine's `CreateSpec` → `WindowHost::window_opened`
  → `DesktopShell::decorate_window`. A resizable-requested window is decorated
  with the resize grabber and a live maximize/restore size toggle; a fixed-size
  app passes `false` and is offered neither (and never receives a `Resized`).
  The mechanism is per-app opt-in, never forced on an app that renders at one
  size (`AGENTS.md` §2.4 — the app decides, the window manager honours it).
- **The file viewer is the shipping resizable app.** `userland/apps/viewer`
  opens `resizable: true`, and on every `WindowEvent::Resized` (an interactive
  grab settling, or a maximize/restore) it allocates a fresh frame region at the
  new client size, `WindowRequest::Resize`s the window onto it, unmaps the old
  region **only after** the session adopts the new one, re-wraps its text to the
  new column count, and repaints — preserving the reader's scroll position. It
  fails closed (keeping the current surface, never crashing) if a new region
  cannot be allocated or the session refuses the re-map. The file manager
  (`resizable: true`) re-lays-out its listing on `Resized`, and the terminal is
  now resizable too (Stage G).
- **The viewer's render is size-parameterized and host-tested.** `render_status`/
  `render_lines` take the current `width`/`height`, `visible_{rows,cols}_for`
  derive the grid from the client size, and `ScrollView::relayout` re-wraps and
  clamps the offset into the resized content — all covered by `tairix_viewer`
  unit tests (arbitrary-size render, geometry scaling, and offset-preserving
  relayout).
- **Tests.** `lib/abi` covers the `resizable` flag round-trip and its dirty-byte
  rejection; `lib/window` covers the flag forwarding to the host; `userland/gui/session`
  covers a resizable-requested open decorating with a resizable frame and a live
  size toggle; the viewer engine covers size-aware render and relayout; the
  freestanding viewer/terminal/files cross-compile against the new signatures.

### Stage G — Resize actually reachable, and in-content pointer input — DONE

Two gaps that made resizable windows only nominally resizable are closed:

- **A resizable window's grab border is invisible.** `WindowFrame`'s
  left/right/bottom band is the 1-pixel `frame_inset` for every window,
  resizable or not (`band_inset`, consumed by `insets`/`layout`): a band wide
  enough to grab showed as dead space around every resizable app's content.
  The grab room lives in the hit map instead — `WindowFrame::hit` reports
  `ResizeEdge` for the client's outermost `hit_slop` pixels, so an edge is
  grabbable while the client stays as large as a fixed window's. The app
  still draws those pixels but does not receive presses on them, the accepted
  trade macOS, GNOME, and Windows make. The frame therefore draws no corner
  grip (there is no band to hold one), and a fixed-size window trades
  nothing: every client pixel reaches it.
- **Client-area pointer motion and release reach the app.** The window manager
  gives a client press an implicit pointer grab (`client_grab`), so the
  subsequent motion (clamped into the client) and the release are delivered to
  the owning app as `WindowEvent::Pointer` `Moved`/`Released` — the missing half
  that left in-content scrollbar thumbs undraggable and tab/combo clicks
  (which complete on release) dead. A hover over client content is delivered
  too, so in-content controls track the pointer. The file manager's own
  scrollbar is now interactive (`tairix_browse::scroll_pointer`: arrow/track
  step, thumb drag, hover), driven through the shared `ScrollBar`.

### Stage H — Bounded resize, bounded move, and decorations that answer the pointer — DONE

Three ways a decorated window could be left unusable are closed, all in the
window manager and the shared furniture:

- **A window cannot be dragged smaller than its own furniture.**
  `TitleBar::min_band_width` is the narrowest band that seats both corner
  clusters with one control extent of drag surface between them, and
  `WindowFrame::min_outer_size` turns it into the smallest outer rectangle
  (that band plus the rim; the bands plus one standard control of client in
  height). The two hard-coded constants the resize-grab used instead
  (`MIN_CLIENT_W`/`MIN_CLIENT_H`) are gone: they had no relation to the
  furniture they were meant to protect, and at 96 px the commands overlapped
  the title long before the clamp bit.
- **An application declares the smallest client it can lay out at**, on the
  existing create request, and the window manager honours the greater of that
  and the furniture's floor (`Compositor::set_window_min_client_size`,
  `window_min_outer_size`). Without it an app that clamps its own layout
  resizes its window back up while the drag keeps shrinking, and the two fight
  once per pointer sample — the visible "the folder bounces as the window
  approaches its minimum" defect. The floor bounds a *user* resize only: an
  application sizing its own window is choosing that size.
- **A dragged window keeps a grabbable patch of its title bar on screen.**
  `TitleBarLayout::drag` publishes the span between the clusters — the move
  surface the bar already laid out — and the move-grab captures it and clamps
  the origin against `screen_rect`: the whole band vertically, and sideways a
  patch as wide as the band is tall. Partly off an edge stays normal; wholly
  unreachable does not. The screen is the whole framebuffer, so a big-desktop
  multi-monitor layout is one region and a window may straddle two monitors.
- **Pointer motion over a decoration reaches it.** The router delivered motion
  to a frame only during a press or grab, so a command button never lit under
  the pointer — the furniture's hover state existed and was unreachable.
  `client_pointer_moved` now hands a furniture-bound sample to that window's
  frame and tells the frame the pointer *left*, so the highlight goes out
  behind it. The plate it draws is the shared `surface_hover` every widget
  button uses (lighter on dark, darker on light), so there is one hover
  definition, not a furniture-specific one. The frame reports its own damage,
  so a sample crossing the drag region still costs nothing.

## 3. Definition of done

- Files — and every other windowed app — is drawn with a title bar
  (title + Close/Minimize/PutToBack/SizeToggle), the frame rim, and grabbable
  resize edges, **without any app drawing its own chrome**. An app crate
  may be changed only to *react* to the WM's typed lifecycle events over the
  existing window path (a minimize notice, a new client size on resize/maximize)
  — never to paint or intercept furniture.
- All furniture is rendered and hit-tested by the WM via `lib/controls::window`;
  the client can neither draw over nor receive input from it.
- Close/Minimize/PutToBack/SizeToggle work cooperatively over the existing
  window path with no new privileged syscall and no ambient authority.
- Dark + light themes, reduced-motion, and high-contrast are all covered;
  damage is confined to furniture on state changes.
- Headless build unaffected; §17.4 layering intact.
- Docs updated and the whole-project gate green (§2.15, §7).
