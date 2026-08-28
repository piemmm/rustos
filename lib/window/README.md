# tairix-window

Stability tier: **experimental**.

The window-channel protocol engine (`plans/APPWIN.md` AW2): the one
definition of the zero-copy, owner-keyed app-window semantics shared by
both ends of the `WINDOW_ENDPOINT` rendezvous, so the desktop session's
server and every app's client can never drift apart.

- **Server** (`WindowServer`): the engine the desktop session composes.
  It decodes each fixed-width `WindowRequest`, attests the in-flight
  caller through the injected `CallerIdentity` seam (the kernel's
  `call_peer_origin` — an unforgeable `ProcId`, never a claimed id),
  keys every window to that owner, maps the app's endpoint-directed
  `shm_grant` region **once** at `Create` through the shared
  `tairix_display::ShmMapper` seam, and hands each `Present` to the
  injected `WindowHost` (the session's compositor bridge) as a
  bounds-checked frame slice plus a validated damage rectangle — no
  per-present mapping, allocation, or copy of its own. A
  `SetBackdropBlur` takes the same owner check and reaches the same
  bridge (`WindowHost::backdrop_blur_set`), so an app frosts the backdrop
  of its own window and of no other. A `Present`, `SetBackdropBlur`, or
  `Close` naming a window the caller does not own is refused `NotFound`
  (no existence oracle); a per-client window cap bounds how much pinned
  memory one app can reserve; a dead client's windows are torn down
  fail-closed via `client_exited`. Routed input is pushed the other way
  with `deliver_event`, which validates the event against the addressed
  window (owner's endpoint, window-local pointer bounds) before handing it
  to the sink. The sink takes the *typed* event, not its wire bytes,
  because only the sink knows whether it goes out now: one that holds an
  event back against a full mailbox folds it by kind and encodes once,
  when it finally goes. A sink that accepts a pick conclusion is answering
  for it either way, which is what clears the pending pick.
- **Client** (`WindowClient` / `WindowEvents`): the app-side half over
  the injected `WindowTransport` seam (the `ipc_call` syscall in
  production). `create` validates and sends the window geometry, grant
  handle, event endpoint, title, and the app's `WindowSizing` — whether it
  is resizable and the smallest client extent it can lay out at, `0`
  declaring none — returning the session-minted window id. The minimum is
  a *declaration*: the window manager enforces it (alongside its own
  furniture floor) so a drag stops there, and an app never clamps a
  granted size by resizing its own window back up, which would fight the
  drag once per pointer sample. `present` sends a frame index plus
  damage, never pixels;
  `set_backdrop_blur` asks for the content behind the window to be
  frosted, a radius in logical pixels with `0` off and anything above
  `WINDOW_BACKDROP_BLUR_MAX_PX` refused at decode; `close` tears the
  window down. `WindowEvents` wraps the injected `EventSource`
  seam — a **parked** wait on the app's own event endpoint, never a
  poll — and decodes each delivered `WindowEvent` fail-closed.
  `present_damage` decides *what* a round presents from the three cases
  every such app faces (`Repaint::Nothing` / `Reported` / `Whole`), and
  `damage_in` clips a reported client-space rectangle onto the window —
  the app's own fail-closed step, since the session refuses a rectangle
  outside the surface. `pointer_point` widens a wire pointer position into
  the signed geometry the controls hit-test in, saturating rather than
  wrapping, so a coordinate past the range hits nothing. A round that changed the view but reported no
  rectangle presents the whole window: over-covering costs pixels, while
  under-covering would leave a stale frame on screen, because the session
  copies only what a present declares. The
  endpoint's *name* (`event_endpoint_for`) and its *depth*
  (`EVENT_MAILBOX_CAPACITY`) are both defined here, once, because both
  ends depend on them agreeing: the session reads a refused delivery as
  evidence that the owner has stopped draining, which would mean
  different things per app if each chose its own slack.
- **Popup surfaces** (`WindowRequest::CreatePopup`, `PopupSpec`): an app
  opens an undecorated child surface above one of its own windows, so a
  context menu or a settings sheet is never clipped by the window that owns
  it. `WindowClient::create_popup` takes one `PopupSpec` — the parent
  window, the grant handle, the event endpoint, the frame count and
  geometry, and an offset in physical pixels from the *parent's client
  origin*, since an app is never told its own window's screen position. The
  server validates it exactly as a `Create` and additionally requires that
  the parent is a live window the caller owns (a foreign or unknown parent
  answers `NotFound`, no existence oracle), then hands it to
  `WindowHost::popup_opened` — the session resolves the parent's screen
  position, adds the offset, and clamps the whole popup onto the screen. A
  popup counts against the **same** per-client window cap, so "popup"
  cannot be used to pin more memory than `Create` may. It carries no title
  and no resizable flag: it is never decorated and never listed on the
  taskbar. `present`, `set_backdrop_blur`, and `close` act on a popup's id
  exactly as on a top-level id; closing the **parent** tears down every
  popup keyed to it (as does `client_exited`), while closing the popup's
  own id tears down only the popup. One `PopupSpec` definition serves both
  halves, so the app's request and the engine's validated view cannot
  drift.
- **The seat's desktop is asked for here, and kept current here.** An app
  cannot draw honestly without knowing the screen it is on, the desktop's
  UI scale, and whether the theme runs light or dark — and the compositor
  that owns all three is another process it must not reach into.
  `WindowClient::desktop` asks for them as one `tairix_abi::desktop::
  DesktopInfo`, before the first window is created so the opening frame is
  already the right size at the right density in the right colours; the
  server answers from the injected `WindowHost::desktop`, which reads the
  live compositor rather than a cached copy. The query is read-only and
  carries no capability: it describes the caller's own seat, names no
  other principal's data, and grants no authority. `Desktop` is the
  app-side holder — it resolves the reported percentage into a `Scale`
  (refusing, never clamping, one outside the range `Scale` admits and
  keeping the last good value), reports the screen as a `Rect`, caps a
  wanted window size to it with `fit_window`, and `apply` adopts a
  `WindowEvent::DesktopChanged` (which the session pushes to every live
  window, `WindowServer::window_ids`) and answers whether anything
  changed. One definition of that bookkeeping, so no app repeats it.
- **The redraw handshake is answered here, not in every app.** The
  session may release a window's retained content to reclaim memory and
  then send `WindowEvent::RedrawRequested`. `WindowClient` remembers each
  window's last presented frame index and current extent, so
  `WindowEvents::wait` re-presents that frame with full-window damage
  before handing the event on — one definition of the answer instead of
  one per app. The event is still delivered, so an app that would rather
  render genuinely fresh pixels can; a window that has never presented
  has nothing to re-send and the event is a no-op; a request naming a
  window this client does not hold is refused like any other foreign
  window. An app rendering in place (single-buffered) may re-present a
  partially drawn frame, which is the same tearing it already accepts
  from rendering in place at all.

The wire format itself lives in `tairix_abi::window_ipc`; this crate adds
the behaviour. Both halves are host-proven in `src/tests.rs` against an
in-process loopback (a real `WindowServer` behind the client seams), so
the request semantics have exactly one tested definition. The production
wiring — the session serving the reserved endpoint from its waitset loop
and the app parking on its event endpoint — lands with the session and
app bundles (`plans/APPWIN.md` AW3/AW4).
