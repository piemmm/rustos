# Desktop session glue

`userland/gui/session` (`rustos-desktop-session`) is the desktop's **session
glue** (`AGENTS.md` §10, `PLAN.md` Stage 7): the component that owns the shared
theme registry and the taskbar model and resolves the taskbar's abstract menu
actions against state the taskbar itself cannot see.

## Why a separate component

The taskbar deliberately owns no theme registry and no spawn capability. When
a start-menu entry is activated it only *reports* an abstract `MenuAction` — a
session control, an application launcher, or the light/dark `ToggleAppearance`.
Resolving that action belongs to the session, which holds the shared
`rustos_theme::ThemeRegistry` and (in later increments) the window-manager and
process capabilities. This crate is that resolver. Its first increment is the
runtime **light/dark switch**.

It composes the other GUI crates and `lib/*` only — `rustos-taskbar` and the
shared `rustos-theme` definition — which is the permitted `userland/gui/*`
edge (`AGENTS.md` §17.4). Nothing outside `userland/gui/*` depends on it
(§17.3), so a headless image omits it cleanly.

## Resolving a taskbar response

`DesktopSession::resolve` turns a `rustos_taskbar::TaskbarResponse` into a
`SessionEvent`:

- Selecting the start menu's appearance-toggle entry
  (`MenuAction::ToggleAppearance`) is the one response the session acts on
  itself. It calls `ThemeRegistry::toggle_appearance`, re-themes the taskbar in
  place, and returns `SessionEvent::AppearanceChanged(ThemeId)`. The embedder
  relays the now-active theme — `DesktopSession::active_theme` — to the window
  manager and the apps.
- Every other response is `SessionEvent::Forward`ed unchanged: a launcher or
  session-control selection, a task activation, a notification or clock press.
  Those need capabilities the session does not hold, so the embedder performs
  them (`AGENTS.md` §10, §16.5).

The session owns both the registry and the `Taskbar`, so a switch is a single
in-place operation rather than a rebuild.

## Switching the theme directly

`toggle_appearance`, `set_theme(ThemeId)`, and `register_theme(Theme)` expose
the same control without going through a menu. `toggle_appearance` and
`set_theme` re-theme the taskbar through one private apply path, so the relay
is never duplicated (`AGENTS.md` §2.2). `set_theme` fails closed with
`ThemeError::UnknownTheme` on an unregistered id, and `register_theme` with
`ThemeError::DuplicateId`, each leaving the active theme and the taskbar
untouched (`AGENTS.md` §5.4 / §2.9).

## Presenting the taskbar through the window manager

The taskbar paints a *rectangular* `rustos_raster::Surface` and the window
manager composites and rounds windows; neither depends on the other
(`AGENTS.md` §17.4). `TaskbarPresenter` is the session's glue between them.
Given a `&mut rustos_wm::Compositor` and the taskbar's own
`rustos_taskbar::TaskbarRenderer` (which holds the across-frame glyph cache),
`present` paints the bar and, while the start menu is open, its popup, and
presents each as a compositor window:

- the bar is placed at `BarLayout::bar`'s origin and rounded with
  `Corners::from_radius(BarLayout::corner_radius)` — the compositor's single
  anti-aliased rounded-corner path, the same one it uses for application
  windows, never a second one (`AGENTS.md` §2.2);
- while the menu is open the popup is placed above the bar at
  `MenuLayout::panel`'s origin and rounded with its `corner_radius`; closing
  the menu removes the popup window.

The presenter owns only the two compositor `WindowId` tokens it minted — the
taskbar model, the renderer, and the compositor are the embedder's, so the
session composes the GUI crates without owning the window-manager handle. It is
total and fails closed (`AGENTS.md` §2.9): a render that cannot allocate its
surface leaves the on-screen window untouched rather than blanking the bar, a
window the compositor no longer knows is re-created on the next present, and
`teardown` removes both windows so a session shutdown leaves nothing orphaned.

## Routing one input stream to both routers

The desktop has two input routers — the window manager's `rustos_wm::InputRouter`
(focus, click-to-activate, interactive move-grabs) and the taskbar's
`rustos_taskbar::TaskbarInput` (start-menu toggle, task activate/minimise,
notification/clock presses) — and both consume the **same** shared
`rustos_input` event vocabulary (`AGENTS.md` §17.4, §2.2). A real input source
produces one event stream, so `SessionInputRouter` is the glue that fans it to
the right router, driven through `handle(event, &mut Compositor, &mut Taskbar)`:

- a **primary press** goes to the taskbar when its menu is open (the menu is
  modal, so a press anywhere selects an entry or dismisses it) or when the
  pointer is over the bar, and to the window manager otherwise — the two never
  both act on one press, so a click on the bar never also activates a window
  beneath it;
- **pointer motion** is fanned to both so their tracked pointer positions stay
  in step, and only the window manager acts on it, dragging a grabbed window;
- a **primary release** goes to the window manager, ending an in-flight
  move-grab (the taskbar ignores releases);
- a non-primary button, or a press/motion neither router acted on, is
  `SessionInputResponse::Ignored`.

Decorations start a title-bar drag through `begin_move`, and the embedder reads
the keyboard owner through `focused`. The router holds no pixels and grants
itself no authority; every routed sub-call is itself total and fails closed
(`AGENTS.md` §2.9).

## Driving the desktop from a live input stream

`DesktopShell` composes the four pieces above — the `DesktopSession`, the
`SessionInputRouter`, the `TaskbarPresenter`, and the `TaskbarRenderer` — into
one event-driven frontend, closing the long-open "feed the router and presenter
from live device events" thread. A real desktop runs a loop: read the pending
pointer events, route each, perform the session-level effect of a taskbar
action, and bring the on-screen bar back in step. `DesktopShell` runs exactly
that loop over an injected `InputSource` seam (a real pointer/keyboard channel
on a running system, an in-memory queue in tests, `AGENTS.md` §7):

- `pump(source, &mut Compositor)` drains every pending event, routing each
  through the `SessionInputRouter` and returning a `ShellOutcome` per event —
  `Ignored`, a `WindowManager` action the embedder may observe, or a
  `Session` event;
- a taskbar action is `resolve`d (the light/dark toggle is applied here, every
  other response forwarded) and the bar is re-presented, so an opened/closed
  menu or a re-themed bar reaches the screen; a window-manager action needs no
  re-present, so motion and drags stay cheap;
- a faulting `InputSource` ends the `pump` with its `Errno`; the events drained
  before the fault stay applied and the embedder replaces or re-polls the
  source (`AGENTS.md` §2.9 / §19.5).

The shell holds no framebuffer and grants itself no authority: the `Compositor`
is the embedder's and is passed in on each call. A loaded notification-icon set
is installed with `set_icons`, a title-bar drag armed with `begin_move`, and
the desktop torn down with `teardown`. A session-level effect the shell cannot
perform with its own state — relaying the switched theme to the WM and apps,
performing a session control, launching an app — is surfaced as a
`ShellOutcome` for the embedder, which holds those capabilities (`AGENTS.md`
§16.5).

Backing the `InputSource` with a live device channel and relaying the
appearance switch to the WM and apps over IPC remain later increments; this
increment is the in-process event loop that ties the routing policy and the
surface glue together.

## Tests

`cargo test -p rustos-desktop-session` covers: the default dark start and the
seeded appearance-toggle entry; resolving the toggle entry flipping dark↔light
and forwarding every other response unchanged without touching the theme;
`set_theme`/`toggle_appearance` relaying the new metrics to the taskbar
(observed through a custom theme with a distinctive corner radius); the
fail-closed `UnknownTheme`/`DuplicateId` paths leaving the taskbar untouched;
and `TaskbarPresenter` placing and rounding the bar, reusing its window across
presents, showing the popup while the menu is open and removing it when it
closes, re-creating a window an embedder removed, relaying a switched theme's
corner radius onto the presented bar, and `teardown` clearing every window. It
also covers `SessionInputRouter`: a press over the bar routing to the taskbar
(even over a window beneath it) while a press over a window or the empty desktop
routes to the window manager, the modal start menu claiming and dismissing an
off-bar press, motion keeping the pointer in step, a window drag continuing
while the pointer is over the bar, a release ending the grab, and a non-primary
press being ignored. Finally it covers `DesktopShell`: `pump` opening the menu
from a press over the start button and presenting the popup, a window press
routing to the window manager without re-presenting the bar, selecting the
appearance-toggle row switching the active theme to light and removing the
closed menu's popup, a faulting `InputSource` returning its `Errno` while the
event drained before it stays applied, a pure motion presenting nothing,
`begin_move` arming a grab on the focused window, `set_icons` installing a
loaded set while the bar still presents, and `teardown` clearing the presented
windows.
