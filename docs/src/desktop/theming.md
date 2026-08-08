# Desktop theming

`lib/theme` (`tairix-theme`) is the single shared theme definition for the
TAIRiX desktop (`AGENTS.md` §6, §10). One theme drives the colours, corner
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

- `Palette` — semantic `Rgba` colour roles: the surface/foreground base
  (`desktop`, `surface`, `surface_raised`, `on_surface`, `on_surface_muted`,
  `accent`, `on_accent`, `border`), the Reactive Alloy control roles
  (`surface_hover`, `surface_pressed`, `rim`, `rim_active`, `danger`), the
  signal roles the boards' legend fixes (the `*_pressure` set,
  `network_activity`, `recovery`, `success`, `warning`, `denied`), and the
  scroll and window-frame roles
  (`scroll_track`, `scroll_thumb`, `frame`). The
  roles are named fields, not a free-form map, so a theme can never omit a
  role and a consumer can never request one that does not exist
  (`AGENTS.md` §2.11). `Palette::signal(SignalRole)` is the one place a
  signal becomes a colour. The window manager, the taskbar, and the apps all
  read these same roles, which is what makes a theme switch apply
  consistently everywhere.
  - `surface_hover` and `surface_pressed` are the pointer plates, and they are
    what makes a **bar-seated** control legible: an icon in the taskbar wears
    no perimeter of its own, so its plate is the only thing that can report
    hover or press. The hover role therefore steps *away* from
    `surface_raised` (the bar fill) in the direction its appearance calls for —
    brighter on dark, deeper on light — and the tests assert that separation on
    both appearances rather than trusting the authored numbers.
  - `frame` is a single *neutral* tone — a step lighter than `surface` on dark,
    a step deeper on light — and it is deliberately **not** a focus signal.
    Every window wears the same quiet rim at every activation, because the rim
    is the line the eye reads a window's shape by: brightening it on focus made
    the boundary the loudest mark on the desktop and left every other window
    looking switched off. Focus is carried by the title bar, whose text sits at
    `on_surface` while active and `on_surface_muted` while not, and under heavy
    contrast the active frame adds a second inner rim line
    (`plans/GUI-CONTROLS-DESIGN.md` §15) so the distinction is a difference in
    shape as well as tone.
- `Metrics` — every logical length the desktop is laid out from, so no
  renderer carries a private constant: the corner radii and
  `border_thickness`; the scrollbar's `scrollbar_breadth` and
  `min_thumb_length`; the control anatomy (`control_height`,
  `control_inset`, `control_gap`, `control_corner_radius`, `seam_thickness`,
  `rail_thickness`, `bead_size`, `measured_thickness`, `progress_thickness`,
  `chart_height`, `selector_extent`, `toggle_track_length`); and the window
  furniture
  (`title_bar_height`, `frame_inset`, `window_control_extent`,
  `resize_grabber_extent`, `hit_slop`).
  - `chart_height` is the one measured instrument that is a *box* rather than
    a line: a history plot needs vertical room to rise and fall in, so it is
    several times `progress_thickness`. A trend confined to a track's
    thickness cannot rise more than a pixel or two whatever it reads.
  - `measured_thickness` is the breadth of a slider's groove and
    `progress_thickness` that of a progress trace's bar. Both are deliberately
    thin instrument lines rather than `control_height` plates, centred in the
    row the owner lays them out in, which is how the boards draw them. The
    trace is the broader of the two: a slider's thumb marks its value, while a
    read-only fill has to stay legible across a long run on its own.
  - `selector_extent` (a checkbox box, a radio circle, a toggle track's
    breadth) and `toggle_track_length` size a boolean selector's *mark*
    smaller than the row that carries it, so the glyph stays compact while the
    full row remains the hit target.
- `Fonts` — one `FontSpec` (family, size, weight) per `TextRole`, derived from
  a single authored base size through the boards' shared ladder (see
  [Typography](#typography)), referencing faces under `/System/Fonts`.
- `CursorSet` — one asset id per `CursorKind`, referencing assets under
  `/System/Graphics`.

`Theme::dark` is the default; `Theme::light` is its light counterpart. Both are
the Reactive Alloy design boards (`plans/desktop1.png`, `plans/desktop2a.png`,
`plans/desktop1-light.png`) read off rather than invented: near-black cool
surfaces (dark) or warm off-white surfaces (light) under one alloy-orange
accent family, with the semantic signal hues the boards' own legend fixes.

The two appearances re-tune every role for their own background — except
`on_accent`, which is deliberately the *same* warm white in both: a primary
action is one treatment in the boards (a warm white label on the alloy-orange
plate), so the token is shared rather than restated (`AGENTS.md` §2.2). The
invariant that matters for it is therefore legibility, not difference, and the
tests assert a minimum luma separation from the accent fill.

## Typography

A theme sizes text by the **job** it does, never by the widget that draws it.
`TextRole` is a closed set — `Display`, `Heading`, `ItemTitle`, `WindowTitle`,
`Body`, `Metric`, `Caption`, `SectionHeader`, `Monospace` — and
`Fonts::spec(role)` is a constant-time array read, so a text draw can neither
miss a lookup nor invent a size literal at the call site.

Every role's size is a percentage of one authored base (body) size, measured
from the design boards, so a theme states *one* number and the whole desktop's
type scales together (`AGENTS.md` §2.2):

| Role | Size | Weight |
| --- | --- | --- |
| `Display` | 250% | Regular |
| `Heading` | 133% | Medium |
| `ItemTitle` | 113% | Medium |
| `WindowTitle` | 100% | Medium |
| `Body` | 100% | Regular |
| `Metric` | 100% | Bold |
| `Caption` | 87% | Regular |
| `SectionHeader` | 80% | Bold |
| `Monospace` | 100% | Regular |

The boards carry their hierarchy with a deliberately *tight* size ladder and a
rising weight — a detail line sits within a point of the title above it, and a
column header is smaller but bold — so weight, not size, does most of the work.
`Display` is the one deliberate exception: it is the rung for a *single*
dominant line on a full screen of its own — the login and lock screens' clock —
and is light rather than heavy, because at that size weight would shout. It has
no place in a window's chrome. Its 250% is the largest multiple that still
rasterises at the maximum authored base under a doubled density; a taller rung
would silently clamp.

The built-ins author the base at 18 logical pixels; `Fonts::ladder` clamps an
authored base into `MIN_BASE_SIZE_PX..=MAX_BASE_SIZE_PX`, so a theme can
neither author text below the rasteriser's legibility floor nor above its
cell-height ceiling (`AGENTS.md` §5.4).

Sizes are *logical* pixels at `tairix_geometry::REFERENCE_DPI`;
`tairix_font::BitmapFont::for_role(fonts, role, scale)` is the one place a role
and the active `Scale` become a rasterised face, so the logical→physical
arithmetic is never duplicated (`AGENTS.md` §10, §2.2).

The shared controls (`lib/controls`) and the shared directory-browser engine
(`lib/browse`) call it *themselves*: neither accepts a face from the
application drawing it, so a control's typography cannot be overridden at a
call site. See
[the control library](../lib/controls.md#the-theme-chooses-the-face-not-the-caller). The weight a role names
is the font service's own `FontWeight`, re-exported rather than restated: the
shipped faces are Regular-only, so `fontd` synthesises the heavier weights as a
bounded thickening of the same outline coverage, leaving the advance — and
therefore every layout — unchanged.

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
present, so there is no unknown-id path to surface. The interactive home of
this control is the **Light / Dark Appearance** pair in the Switchboard
capsule's quick-actions menu (`plans/NEW-TASKBAR.md` T13): the taskbar reports
the chosen appearance as a typed response and the session glue
(`userland/gui/session`, `tairix-desktop-session`) resolves it through
`DesktopSession::set_theme`, which re-applies the new theme — the taskbar is
re-themed in place and the window manager's desktop background is re-coloured
through the compositor's runtime `set_background` (full-screen damage, so the
next present repaints every pixel over the new colour). The active appearance
carries a check bead in the menu and is not actionable, so the menu can never
ask for the appearance already in use. See
[Desktop session glue](./session.md) and [Taskbar](./taskbar.md).

### Open application windows follow the switch

An application's window is the application's own pixels: the session composes
them but cannot re-colour them, so re-theming the desktop alone would leave
every open window sitting in the appearance the user just left. The session
therefore *tells* each one. `Appearance` is part of the seat's desktop record
(`tairix_abi::desktop::DesktopInfo`), which an app reads before it paints its
first frame and is sent again, as a `WindowEvent::DesktopChanged`, to every
live window whenever the switch happens. Each app re-applies the appearance to
its own `ThemeRegistry`, re-resolves whatever it derived from the theme, and
presents — so the switch reaches the whole screen at once. The enum crossing
that wire *is* `tairix_theme::Appearance`: the theme crate re-exports the ABI's
definition rather than restating it, so the byte on the wire and the value a
theme carries cannot drift apart. See
[Variable DPI and UI scale](./dpi.md) for the rest of that record.

What crosses that wire is the appearance *axis*, not a palette. That is exact
today, because the desktop's own switch chooses between the two built-ins and
nothing registers a third theme at runtime, so naming the axis names the
theme. A desktop that did activate a custom registered palette would leave an
app drawing its own built-in of the same appearance: closing that would mean
either sending the palette itself over the window channel or giving apps a
themes service to read, and neither is worth its cost while the built-in pair
is the whole of what a user can select. Registering a custom theme therefore
means answering that question first, not adding a call.

## Tests

`cargo test -p tairix-theme` covers the built-in palettes (every
appearance-dependent role differs between dark and light, `on_accent` is shared
and stays legible on the accent fill, body and muted text clear their own
minimum contrast, the hover and pressed plates each separate from the bar fill
in their appearance's direction, surfaces are opaque and distinct), the type
ladder (every role's size and weight, the descending order, the base-size clamp
at both ends, and the monospace role being the only one on the fixed-width
family), the shared metrics/fonts/cursors, cursor lookup for every kind, and
the registry: the dark default, runtime dark↔light switching, custom-theme
registration and activation, and the fail-closed `UnknownTheme`/`DuplicateId`
paths. The
window manager's `cargo test -p tairix-wm` adds the integration tests that
source the compositor background and a window's corner radius from the active
theme and verify a dark→light switch changes the cleared screen. The
appearance-toggle tests cover `set_appearance` selecting the matching built-in
and `toggle_appearance` flipping between built-ins (including from an active
custom theme).
