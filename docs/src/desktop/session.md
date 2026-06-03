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
  them (`AGENTS.md` §10, §16.5). (`DesktopShell` additionally applies a task
  activation's window effect to the compositor itself — see *Running-task list
  ↔ window stack* — before forwarding it.)

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

## Running-task list ↔ window stack

The taskbar models a running-task list — one entry per top-level window, with
the click-to-activate / minimise rule — but owns no window manager, and the
window manager owns no task list (`AGENTS.md` §17.4). `TaskBridge` is the glue
between them. A task is named by a `rustos_taskbar::TaskId` and a window by an
opaque `rustos_wm::WindowId`, so the bridge owns the correspondence: it mints a
stable task id per window it tracks and never reuses one, then translates
between the two whenever the bar acts on a window or the window manager moves
focus. Each operation is total and fails closed (`AGENTS.md` §2.9):

- `open` adds a window to the compositor, lists it as a running task, and shows,
  raises, and focuses it (a freshly opened window takes focus); it opens nothing
  only if the task-id space is exhausted;
- `close` removes the window from the compositor and its task from the bar,
  dropping focus if the closed window held it; an untracked window is a no-op;
- `activate` applies the bar's `ActivateOutcome` to the compositor — an
  activated task is shown, raised, and focused; a minimised one is hidden and,
  if it held focus, unfocused — and is a no-op for an unknown task;
- `sync_focus` mirrors a window-manager focus change (the user clicked a window
  directly, or pressed the desktop) back into the bar's highlight, returning
  whether the highlight moved so a click on a window that owns no task neither
  blanks the highlight nor forces a needless repaint.

`DesktopShell` drives the bridge: `open_window` / `close_window` manage the
lifecycle, `handle` applies a `TaskActivated` outcome to the compositor and
mirrors a window-manager focus change into the bar, and the focus move uses the
window manager's new `InputRouter::focus` / `unfocus` (validated against the
compositor, fail-closed) so focusing a task by id keeps the keyboard owner in
step. The bridge holds no pixels and grants itself no authority — the
compositor, the router, and the taskbar are the embedder's, passed in per call.

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
- a taskbar action is `resolve`d (the light/dark toggle is applied here, a
  task activate/minimise outcome is applied to the compositor, every other
  response forwarded) and the bar is re-presented, so an opened/closed menu, a
  re-themed bar, or a changed task highlight reaches the screen; a
  window-manager action re-presents only when it moved focus between tasks,
  so motion and drags stay cheap;
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

## Live device input source

The `InputSource` the shell `pump`s is now backed by a live device channel.
`DeviceInputSource` (the `device` module) wraps an injected
`PointerInputChannel` seam — a capability-checked kernel input channel on a
running system, an in-memory queue in tests (`AGENTS.md` §7) — that hands the
desktop one framed `rustos_abi::input::PointerInput` record at a time. Each
`poll` reads one record and decodes it through `PointerInput::from_bytes` into
the `lib/input` `InputEvent` the window manager and taskbar route: a `Moved`
record becomes an absolute `PointerMoved`, a `Pressed` / `Released` record
becomes a `PointerPressed` / `PointerReleased` carrying the resolved
`PointerButton`. The crate holds no input capability of its own — the channel
delivers the bytes and the decode runs above the device (`AGENTS.md` §17.4 /
§19.5) — and a malformed record fails closed with its `Errno` rather than being
misinterpreted, ending the shell's `pump` without disturbing the events already
applied (`AGENTS.md` §5.4 / §2.9). The ABI record itself is the desktop-level
pointer event documented in [Input events](../abi/input.md); it is a
distinct layer from the device-level driver input ABI, not a duplicate of it
(`AGENTS.md` §2.2).

## Live keyboard input source

The keyboard's live backing is `KeyboardInputSource` (the `keyboard` module),
the counterpart of `DeviceInputSource`. It wraps an injected `KeyInputChannel`
seam — a capability-checked kernel keyboard channel on a running system, an
in-memory queue in tests (`AGENTS.md` §7) — that hands the desktop one framed
`rustos_abi::input::KeyInput` record at a time. Each `poll` decodes one record
through `KeyInput::from_bytes` into the same `lib/input` `InputEvent` stream the
shell pumps: a `Pressed` / `Released` record becomes a `KeyPressed` /
`KeyReleased` carrying the resolved `Key` (a produced `Char`, or a `NamedKey` —
the wire ABI's twelve function-key codes fold into one `NamedKey::Function`)
and the held `Modifiers`. The `SessionInputRouter` routes a key to the window
manager, which delivers it to the focused window; the taskbar takes no keyboard
input. As with the pointer the crate holds no input capability of its own, and
a malformed record fails closed with its `Errno` rather than being
misinterpreted (`AGENTS.md` §5.4 / §2.9). The ABI record is documented in
[Input events](../abi/input.md).

Relaying the appearance switch to the WM and apps over IPC remains a later
increment; the desktop now reads a live pointer **and** keyboard event stream
end to end.

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
from a press over the start button and presenting the popup, a window press on a
window that owns no task routing to the window manager without re-presenting the
bar, selecting the appearance-toggle row switching the active theme to light and
removing the closed menu's popup, a faulting `InputSource` returning its `Errno`
while the event drained before it stays applied, a pure motion presenting
nothing, `begin_move` arming a grab on the focused window, `set_icons`
installing a loaded set while the bar still presents, and `teardown` clearing
the presented windows. It covers `TaskBridge` end to end through the shell:
`open_window` listing, focusing, and presenting a new task; `close_window`
removing the task and dropping focus (and a second close being a no-op);
clicking a task slot minimising the window and dropping focus, then a second
click restoring and re-focusing it; clicking a window directly moving both the
window-manager focus and the bar highlight; pressing the desktop clearing the
highlight while the task stays listed; the window↔task mapping both ways;
activating an unknown task changing nothing; and syncing focus to an untracked
window leaving the highlight in place while clearing it on a desktop press. It
also covers `DeviceInputSource`: decoding a `Moved` record to an absolute
`PointerMoved`, each pointer button for press and release, a malformed
(all-zero) record surfacing `BadMagic` rather than being misinterpreted, a
channel fault propagating while a queued record still decodes afterwards, and
`into_channel` returning the wrapped channel. It covers `KeyboardInputSource`
likewise: decoding a character press with its modifiers, a named release that
folds a function key into `NamedKey::Function`, a malformed record surfacing
`BadMagic`, a channel fault propagating while a queued record still decodes,
and `into_channel`.
