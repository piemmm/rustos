# Desktop icons

TAIRiX status and notification icons are **vectorised, scalable, and
themeable** — scalable vector artwork rather than fixed-resolution bitmaps
(`AGENTS.md` §6 / §10, `PLAN.md` Stage 7). They live in the shared `lib/icon`
crate (`tairix-icon`) so the taskbar draws them without the taskbar and the
window manager depending on one another (`AGENTS.md` §17.4). The crate is
`no_std`, `#![forbid(unsafe_code)]`, and owns no scan converter or colour
arithmetic of its own — exactly like `lib/cursor`.

## The vector representation

An icon is a `VectorIcon`: an ordered stack of filled `IconLayer`s over a
square design grid. Each layer is a single polygon ring (a list of `(x, y)`
design-grid coordinates) and a straight-alpha fill colour. A multi-part glyph
— a battery body plus its terminal, a bell plus its clapper — is built by
stacking layers, not by a second multi-contour scan converter.

- **Scaling** is exact: `VectorIcon::rasterise(side)` renders the design grid
  across a fresh `side`×`side` `Surface`, transparent everywhere the glyph
  does not draw. `draw_onto` composites onto a surface the caller already
  owns.
- **Anti-aliasing** and the blend come from one place — `lib/raster`'s
  `Surface::fill_polygon`, the same supersampled polygon path the cursor
  library uses. The icon library hands its layers to that shared path, so the
  desktop has exactly one polygon rasteriser (`AGENTS.md` §2.2 / §10).
- **Theming** is a single colour: each built-in glyph is a monochrome
  silhouette tinted by a colour the caller supplies from the active theme, so
  re-theming is data rather than new code.

A zero `side` or an unallocatable buffer fails closed with `None` rather than
panicking (`AGENTS.md` §2.9).

## The glyph set

`IconKind` is the closed set of built-in glyphs: the status kinds (`Network`
— rising signal bars, `Volume`, `Battery`, `Bell`), the file-manager kinds
(`Folder`/`FolderFilled`/`FolderOpen` — an empty folder, one that holds
something, and an open one — `File`, `AppBundle`, the type badges
`Text`/`Image`/`Archive`/`Executable`, the toolbar's `NavBack`/`NavForward`/`NavUp`/
`Refresh`/`ViewToggle`/`Sort`/`NewFolder`, and `Trash`/`EmptyTrash`), the
taskbar's `Library` (the program-library launcher's three-by-three tile
grid, `plans/NEW-TASKBAR.md` T4) and `Switchboard` (its always-trailing tray
capsule), `ListMenu` (three bulleted bars, for the button that opens a
screen's own section list beside a location breadcrumb,
`plans/NEW-SWITCHBOARD.md` S2), the task-command kinds the Switchboard's
Tasks section names its commands with — `Job` (an hourglass, for queued or
background work), `TaskSwitch` (an arrow entering a window body),
`Reveal` (the window frame itself), `Pause`, `Resume`, `Priority` (a
downward arrow) and `Quit` (a cross) — and a `Generic` fallback diamond.
`IconKind::for_asset` resolves a theme asset identifier to a kind and
falls back to `Generic` for an unrecognised id, so an unexpected notification
still draws a placeholder instead of nothing (`AGENTS.md` §2.9).
`builtin_icon(kind, colour)` turns a kind plus a theme colour into a
`VectorIcon`; `ICON_KINDS` is the closed table a loader iterates.

On-disk icon sets follow the desktop's **SVG-first** asset rule (`AGENTS.md`
§10), the same as cursors: a set under `/System/Graphics` is authored as SVG
and decoded — through the curated §16.4 image-decoding library (`lib/svg`) in
a §19.5 parser sandbox — into the in-memory `VectorIcon` form shown here.
`tairix_icon::decode_svg(bytes)` (built on `tairix_svg::decode` and
`VectorIcon::from_svg`) performs that conversion; a malformed or out-of-subset
asset fails closed, so the caller substitutes a `builtin_icon` glyph rather
than crashing (`AGENTS.md` §2.9). See [SVG asset decoding](./svg-assets.md).
The built-in glyphs remain the always-present fallback.

SVG is likewise the preferred form for an **application bundle's own** icon,
which every app must ship (`plans/APPS.md` §14) and which decodes through this
same path. The complete order a request resolves through — a thing's own icon,
then its class's raster master, then the class vector asset, then the built-in
glyph — is described under [tairix-icon](../lib/icon.md).

## In the taskbar

The taskbar's notification area holds an ordered list of status icons, each
naming a theme asset id. When the bar renders, every notification slot resolves
its asset id to an `IconKind`, builds a `VectorIcon` in the theme's
`on_surface_muted` foreground colour, rasterises it to the slot size at the
active scale, and composites it onto the bar through `lib/raster`'s
`Surface::blit`. The glyph is artwork, not a flood fill: the raised bar
background shows through around it. A slot too small to hold a glyph, or an
unrenderable size, paints nothing rather than panicking (`AGENTS.md` §2.9).
