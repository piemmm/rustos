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

## Loading the on-disk graphics assets

The desktop's cursors and notification icons are authored as SVG under
`/System/Graphics` (the SVG-first asset rule, `AGENTS.md` §10 / §16.2).
`lib/cursor` and `lib/icon` own the decode-and-fall-back logic but stay
`no_std` with no path of their own; reading the bytes needs a filesystem
capability, so it is the session's job (`AGENTS.md` §17.4 / §19.5). The
`assets` module is that job:

- A caller supplies a `GraphicsAssetReader` (VFS-backed on a running system,
  an in-memory table in tests).
- `DesktopSession::load_cursors` reads one asset per cursor kind named by the
  active theme's `CursorSet`, from
  `/System/Graphics/Cursors/<asset-id>.svg`, and returns a `CursorTheme` the
  window manager registers through its `CursorRegistry`.
- `DesktopSession::load_icons` reads one asset per icon kind, from
  `/System/Graphics/Icons/<asset-id>.svg`, and returns an `IconSet` the
  taskbar installs through `TaskbarRenderer::set_icons`.

Both are **total and fail-closed per kind** (`AGENTS.md` §2.9): a kind whose
asset is missing, unreadable, malformed, or out of subset keeps its built-in
artwork, so a corrupt or absent `/System/Graphics` can never blank the
pointer or a status icon — it simply yields the built-in set.

## Presenting the taskbar through the window manager

`TaskbarPresenter` joins the taskbar to the compositor. The taskbar paints a
*rectangular* `rustos_raster::Surface` and the window manager composites and
rounds windows; neither depends on the other (`AGENTS.md` §17.4), so the join
is session glue. Given a `&mut rustos_wm::Compositor` and the taskbar's own
`TaskbarRenderer` (which holds the across-frame glyph cache), `present`:

- paints the bar, places it at `BarLayout::bar`'s origin, and rounds it with
  `Corners::from_radius(BarLayout::corner_radius)` — the compositor's single
  anti-aliased rounded-corner path, the same one it uses for application
  windows, never a second one (`AGENTS.md` §2.2);
- while the start menu is open, paints its popup, places it above the bar at
  `MenuLayout::panel`'s origin, and rounds it the same way; closing the menu
  removes the popup window.

The presenter owns only the two compositor `WindowId` tokens it minted, so the
session composes the GUI crates without holding the window-manager handle. It
is total and fails closed (`AGENTS.md` §2.9): a render that cannot allocate
leaves the on-screen window untouched, a window the compositor no longer knows
is re-created on the next present, and `teardown` removes both windows.

## Dependencies and layering

The crate composes the other GUI crates and `lib/*` only — `rustos-taskbar`,
`rustos-wm`, and the shared `rustos-theme` definition, plus `rustos-cursor` /
`rustos-icon` (the SVG set builders) and `rustos-abi` (the `Errno` the read
seam returns) (`AGENTS.md` §17.4). Composing GUI crates is the permitted
`userland/gui/*` edge; nothing outside `userland/gui/*` depends on it (§17.3),
so a headless image omits it cleanly.

It is `no_std`. `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).

## Still to come (Stage 7)

Relaying the active theme to the window manager and apps over live IPC,
relaying live pointer/keyboard events into the taskbar's input router (the
`TaskbarPresenter` surface glue now exists), resolving launcher /
session-control actions once the process and window-manager capabilities are
wired (deferred Stage 6 work), and the VFS-backed `GraphicsAssetReader` that
reads `/System/Graphics` on a running system (the in-memory-tested loader and
its fallbacks now exist).
