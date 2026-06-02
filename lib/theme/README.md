# rustos-theme

The single shared desktop **theme definition** for RustOS (`AGENTS.md` §6,
§10 — `PLAN.md` Stage 7). One theme drives the colours, corner radii, fonts,
and cursors of the window manager, the taskbar, and the default apps, with a
default dark theme and a light theme switchable at runtime.

This crate is pure theme *data*. A `Theme` is a table of:

- `Palette` — semantic `Rgba` colour roles (`desktop`, `surface`,
  `surface_raised`, `on_surface`, `on_surface_muted`, `accent`, `on_accent`,
  `border`). Roles are fixed fields, so a theme can never omit one and a
  consumer can never ask for one that does not exist (`AGENTS.md` §2.11).
- `Metrics` — corner radii (window, taskbar, popup) and border thickness,
  the data the window manager's single anti-aliased rounded-corner path
  consumes (`AGENTS.md` §2.2).
- `Fonts` — the UI and monospace `FontSpec`s, referencing faces under
  `/System/Fonts`.
- `CursorSet` — one cursor asset id per `CursorKind`, referencing assets
  under `/System/Graphics`.

The crate owns no rendering or compositing arithmetic — that lives in the
shared rasteriser `lib/raster`. A consumer converts a theme `Rgba` into the
shared render colour at the edge (`lib/raster` provides `From<Rgba> for
rustos_raster::Color`), so the colour algebra is never duplicated
(`AGENTS.md` §2.2).

`ThemeRegistry` owns the available themes and the active selection. It always
holds the two built-ins (so there is always an active theme), switches with
`set_active`, and accepts custom themes with `register` — adding a theme is
data, not code (`AGENTS.md` §10). Both mutators fail closed: an unknown id or
a duplicate id returns a `ThemeError` and changes nothing (`AGENTS.md` §5.4 /
§2.9).

The runtime light/dark control toggles the `Appearance` axis rather than a
specific id: `set_appearance(Appearance)` activates the matching built-in and
`toggle_appearance` flips to the opposite built-in based on the active theme's
appearance. Both return the now-active `ThemeId` and cannot fail (the
built-ins are always present), so there is no error path to surface.

## Why it lives in `lib/`

Sibling userland crates may not depend on one another (`AGENTS.md` §17.4), so
the one definition the GUI crates and apps all read belongs in `lib/*`,
exactly as `lib/procinfo` is the shared home for the System Information
client helpers. The crate has no dependencies and sits at the bottom of the
§17.4 layering: it is depended on, never depends.

## Stability tier

`experimental` — the surface is the Stage 7 desktop theming seam, consumed
first by `userland/gui/wm` and, in later increments, by the taskbar and the
default apps. It is `no_std` (with `alloc`) and has no dependencies. No
`unsafe`, and no `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md`
§2.9).
