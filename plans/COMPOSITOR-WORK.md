# COMPOSITOR-WORK.md — Window decorations in the compositor (server-side furniture)

This is the staged build plan for giving every windowed app real window
decorations — a title bar with Close / Minimize / PutToBack / SizeToggle
controls, an active/inactive frame rim, and a resize grabber — by rendering
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

**Status:** Stages A–F are **done**. Server-side window decorations are live:
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

- Each decorated `Window` keeps a pre-rendered, outer-sized decoration
  `Surface` (`window.rs::render_decoration`), painted through
  `WindowFrame::render`/`TitleBar::render` (rim, body, the sanitised title via
  `lib/font`, the four `WindowControl` buttons) plus a corner `ResizeGrabber`,
  using the one `lib/raster` fill and the shared rounded-corner path (no second
  recipe). The rim's rounded corners stay transparent so the desktop shows
  through.
- `Window::sample_local` samples that decoration in the reserved band and the
  inset client content inside it, so both the software composite and the
  hardware-accelerated `encode_layers` path draw the furniture identically; the
  client never overlaps the band.
- The title the WM receives on the channel (`WindowTitle`) is rendered in the
  title bar via `Compositor::set_window_title`, not merely used as the taskbar
  label.
- Active/inactive rim treatment follows the focused window through
  `Compositor::set_active_frame`; attention requests are preserved rather than
  clobbered by a focus change.
- A focus change or title edit repaints only the furniture bands
  (`Window::furniture_bands`/`title_band`), never the client — damage stays
  confined to the furniture.
- Tests cover dark and light theme render, active vs inactive rim, the title
  being drawn, reduced-motion pixel-identical render, high-contrast glyph
  thickening, and furniture-confined damage on a focus flip and a title edit.

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
  `Compositor::resize_window` (content surface reallocated, pixels preserved,
  origin + decoration following), reporting `Resized`/`ResizeEnded`; Escape
  cancels and restores the pre-drag geometry exactly.
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
  entry); the engine's `WindowHost::window_resized` reallocates the compositor's
  content surface (`Compositor::resize_window_client`). An interactive
  resize-grab (`ResizeEnded`) forwards the settled client size to the app the
  same way, once, at the end of the drag.
- **Force-quit** is **not** a title-bar control — it remains the separate
  capability-checked recovery path.
- **Resizability is per-window, and decorations are still off in the running
  desktop.** The mechanism (grabber, size-toggle, resize protocol) is complete
  and tested, but no served window opts into a frame yet, so — as with Stages
  A–C — there is no behavioural change in the live session. Turning decorations
  on (and deciding which apps present as resizable, re-rendering on `Resized`) is
  Stage E. The default apps present fixed-size windows today and treat
  `Minimized`/`Resized` as honest no-ops (a fixed-size window the WM decorates
  non-resizable never receives a size change); a future resizable app handles
  `Resized` by re-mapping its region via `WindowClient::resize`.
- **Tests** cover: Close yields `CloseRequested` for the owning window and
  nothing for a non-served window; a resize/close/present against a foreign or
  dead window is refused fail-closed (`lib/window`); minimize hides the window +
  marks the taskbar entry + emits `Minimized`; put-to-back restacks with no
  event; size-toggle maximizes to the work area then restores and emits
  `Resized`; and the engine `Resize` re-maps the region and the host reallocates
  the surface.

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

- **A resizable window reserves a real grab border.** `WindowFrame`'s
  left/right/bottom band is the theme's `resize_grabber_extent` for a
  **resizable** window (`band_inset`, consumed by `insets`/`layout`), instead
  of the 1-pixel `frame_inset` a fixed window keeps. The corner `ResizeGrabber`
  and the frame resize edges now sit in furniture the pointer can actually hit,
  and the client insets out from under the grabber (it never overlaps content).
  A fixed-size window is unchanged.
- **Client-area pointer motion and release reach the app.** The window manager
  gives a client press an implicit pointer grab (`client_grab`), so the
  subsequent motion (clamped into the client) and the release are delivered to
  the owning app as `WindowEvent::Pointer` `Moved`/`Released` — the missing half
  that left in-content scrollbar thumbs undraggable and tab/combo clicks
  (which complete on release) dead. A hover over client content is delivered
  too, so in-content controls track the pointer. The file manager's own
  scrollbar is now interactive (`tairix_browse::scroll_pointer`: arrow/track
  step, thumb drag, hover), driven through the shared `ScrollBar`.

## 3. Definition of done

- Files — and every other windowed app — is drawn with a title bar
  (title + Close/Minimize/PutToBack/SizeToggle), an active/inactive frame rim,
  and a resize grabber, **without any app drawing its own chrome**. An app crate
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
