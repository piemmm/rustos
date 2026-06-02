# Desktop theming

`lib/theme` (`rustos-theme`) is the single shared theme definition for the
RustOS desktop (`AGENTS.md` §6, §10). One theme drives the colours, corner
radii, fonts, and cursors of the window manager, the taskbar, and the default
apps, with a default dark theme and a light theme switchable at runtime.
"Adding a theme is data, not new code" (`AGENTS.md` §10).

## Why a `lib/` crate

Sibling userland crates may not depend on one another (`AGENTS.md` §17.4), so
the one definition the GUI crates and apps all read lives in `lib/*` — the
same reasoning that puts the System Information client helpers in
`lib/procinfo`. The crate has no dependencies and sits at the bottom of the
§17.4 layering: it is depended on, never depends.

## A theme is data

The crate owns no rendering arithmetic; it is a table of values. A `Theme`
bundles, under a stable `ThemeId`:

- `Palette` — semantic `Rgba` colour roles: `desktop`, `surface`,
  `surface_raised`, `on_surface`, `on_surface_muted`, `accent`, `on_accent`,
  and `border`. The roles are named fields, not a free-form map, so a theme
  can never omit a role and a consumer can never request one that does not
  exist (`AGENTS.md` §2.11). The window manager, the taskbar, and the apps
  all read these same roles, which is what makes a theme switch apply
  consistently everywhere.
- `Metrics` — `window_corner_radius`, `taskbar_corner_radius`,
  `popup_corner_radius`, and `border_thickness`.
- `Fonts` — a `ui` and a `monospace` `FontSpec` (family, size, weight),
  referencing faces under `/System/Fonts`.
- `CursorSet` — one asset id per `CursorKind`, referencing assets under
  `/System/Graphics`.

`Theme::dark` is the default; `Theme::light` is its light counterpart.

## No duplicated colour algebra

A theme `Rgba` is a straight-alpha colour token with no compositing
arithmetic. The premultiplied-alpha blending lives in the shared rasteriser
`lib/raster` (re-exported by the window manager and used by the taskbar). The
two meet at exactly one edge — `lib/raster`'s `From<Rgba> for Color`
conversion — so the colour algebra is never copied into the theme crate, nor
re-implemented per consumer (`AGENTS.md` §2.2). Likewise a window or the
taskbar derives its corner style from a theme radius through the compositor's
single rounded-corner path with `Corners::from_radius` (radius `0` is the
square opt-out), never a second rounding implementation.

## Switching at runtime

`ThemeRegistry` owns the available themes and the active selection.
`ThemeRegistry::with_builtins` always holds the dark and light themes (so
there is always an active theme), `set_active` switches the active theme by
id, and `register` adds a custom theme. Both mutators fail closed
(`AGENTS.md` §5.4 / §2.9): `set_active` on an unknown id and `register` of a
duplicate id each return a `ThemeError` and leave the registry unchanged.
Because the built-ins are held in a fixed-size array, `active` always returns
a theme without an `unwrap` or an out-of-bounds index.

### The light/dark control

A "switch to light/dark" desktop control toggles the `Appearance` axis, not a
specific id. `ThemeRegistry::set_appearance(Appearance)` makes the built-in of
the requested appearance active, and `toggle_appearance` flips to the opposite
built-in based on the *active* theme's `Appearance` (a custom dark theme
toggles to the light built-in, and vice versa). Both return the now-active
`ThemeId`. Unlike `set_active` they cannot fail: the two built-ins are always
present, so there is no unknown-id path to surface. The taskbar's start menu
surfaces this control as a `MenuAction::ToggleAppearance` entry; the taskbar
holds no registry, so it only reports the action and the session glue performs
the switch and re-applies the new theme to the window manager, taskbar, and
apps.

## Tests

`cargo test -p rustos-theme` covers the built-in palettes (every role
differs between dark and light, surfaces are opaque and distinct), the shared
metrics/fonts/cursors, cursor lookup for every kind, and the registry: the
dark default, runtime dark↔light switching, custom-theme registration and
activation, and the fail-closed `UnknownTheme`/`DuplicateId` paths. The
window manager's `cargo test -p rustos-wm` adds the integration tests that
source the compositor background and a window's corner radius from the active
theme and verify a dark→light switch changes the cleared screen. The
appearance-toggle tests cover `set_appearance` selecting the matching built-in
and `toggle_appearance` flipping between built-ins (including from an active
custom theme).
