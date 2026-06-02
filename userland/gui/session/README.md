# rustos-desktop-session

The RustOS desktop **session glue** (`AGENTS.md` §10, `PLAN.md` Stage 7): the
component that owns the shared theme registry and the taskbar model and ties
the desktop's parts together.

The taskbar deliberately owns no theme registry and no spawn capability:
activating a start-menu entry only *reports* an abstract `MenuAction` (a
session control, an application launcher, or the light/dark
`ToggleAppearance`). Resolving that action is the session glue's job. This
crate is that glue's first increment — the runtime **light/dark switch**.

## What this crate owns

- **The shared `rustos-theme` `ThemeRegistry`** — the one runtime registry the
  whole desktop reads its theme from (`AGENTS.md` §6, §10).
- **The `rustos-taskbar` `Taskbar` model** — so a theme switch is a single
  in-place operation: the registry's active theme changes and the taskbar is
  re-themed to match.

## What it does

`DesktopSession::resolve` turns a `TaskbarResponse` into a `SessionEvent`:

- A selection of the start menu's appearance-toggle entry is the one response
  the session acts on itself. It switches the built-in light/dark theme on the
  registry (driven by the *active* theme's appearance, so a custom dark theme
  toggles to the built-in light theme and vice versa), re-themes the taskbar,
  and returns `SessionEvent::AppearanceChanged(ThemeId)`. The embedder relays
  the now-active theme — `DesktopSession::active_theme` — to the window manager
  and apps.
- Everything else is `SessionEvent::Forward`ed unchanged: a launcher or
  session-control selection, a task activation, a notification or clock press.
  Those need capabilities the session does not hold (a window-manager handle,
  the power/spawn capabilities), so the embedder performs them (`AGENTS.md`
  §10, §16.5).

`toggle_appearance`, `set_theme`, and `register_theme` expose the same theme
control directly. `toggle_appearance` and `set_theme` re-theme the taskbar
through one private apply path, so the relay logic is never duplicated
(`AGENTS.md` §2.2). `set_theme` fails closed on an unknown id and
`register_theme` on a duplicate id, leaving the active theme and the taskbar
untouched (`AGENTS.md` §5.4 / §2.9).

## Dependencies and layering

The crate composes the other GUI crates and `lib/*` only — `rustos-taskbar`
and the shared `rustos-theme` definition (`AGENTS.md` §17.4). Composing GUI
crates is the permitted `userland/gui/*` edge; nothing outside
`userland/gui/*` depends on it (§17.3), so a headless image omits it cleanly.

It is `no_std`. `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).

## Still to come (Stage 7)

Relaying the active theme to the window manager and apps over live IPC,
presenting and placing the start-menu popup surface through the window
manager, and resolving launcher / session-control actions once the process
and window-manager capabilities are wired (deferred Stage 6 work).
