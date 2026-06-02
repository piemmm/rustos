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

## Tests

`cargo test -p rustos-desktop-session` covers: the default dark start and the
seeded appearance-toggle entry; resolving the toggle entry flipping dark↔light
and forwarding every other response unchanged without touching the theme;
`set_theme`/`toggle_appearance` relaying the new metrics to the taskbar
(observed through a custom theme with a distinctive corner radius); and the
fail-closed `UnknownTheme`/`DuplicateId` paths leaving the taskbar untouched.
