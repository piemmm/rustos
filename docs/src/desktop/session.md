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

Relaying live pointer/keyboard events into the taskbar's input router and the
appearance switch to the WM and apps over IPC remain later increments; this
increment is the surface-presentation glue.

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
corner radius onto the presented bar, and `teardown` clearing every window.
