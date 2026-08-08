# tairix-theme

The single shared desktop **theme definition** for TAIRiX (`AGENTS.md` §6,
§10 — `PLAN.md` Stage 7). One theme drives the colours, corner radii, fonts,
and cursors of the window manager, the taskbar, and the default apps, with a
default dark theme and a light theme switchable at runtime.

This crate is pure theme *data*. A `Theme` is a table of:

- `Palette` — semantic `Rgba` colour roles (`desktop`, `surface`,
  `surface_raised`, `on_surface`, `on_surface_muted`, `accent`, `on_accent`,
  `selection_fill`, `border`). Roles are fixed fields, so a theme can never
  omit one and a consumer can never ask for one that does not exist.
  `selection_fill` is the plate a selected item is filled
  with — each theme's own `accent` at three tenths opacity (`#d1550f4d` on dark,
  `#c8500c4d` on light), authored per theme so a theme can tune its weight
  rather than have it derived at the draw site. It is this light because the
  frosted backdrop under a selected item is what marks it, leaving the accent
  to tint rather than to cover.
- `Metrics` — corner radii (window, taskbar, popup) and border thickness,
  the data the window manager's single anti-aliased rounded-corner path
  consumes, plus `selection_backdrop_blur`, how far the *backdrop* behind a
  selected item is blurred, in logical pixels (`19` in both themes). The fill
  itself keeps a crisp edge; the pixels it covers — a window's surface, the
  wallpaper — are frosted through the same filter the compositor frosts a
  window's backdrop with, at three tenths of the widest blur a window may ask
  for.
- `Timeline` — one animation in flight: the single definition of how a theme's
  duration becomes frames. A surface starts one for the interaction it is
  beginning, reads `progress` (linear, for a strength fade) or `eased`
  (smoothstep, for anything that travels) when it paints, and reads
  `next_frame_in` to know when to wake — the nearer of what remains and one
  frame at 60 Hz. It reads no clock of its own; the embedder passes the
  monotonic instant it already holds. A zero duration starts *settled*:
  complete, asking for no wake, so reduced motion needs no second code path
  and an idle surface arms no timer.
- `MotionTheme` — one duration per `MotionInteraction`, in milliseconds, held
  as a table indexed by the interaction so a duration can never be transposed
  onto the wrong one. `StageTransition` (`240` ms) is one view giving way to
  another, `AttemptRejected` (`420` ms) the shake on a refused attempt, and
  `SessionFade` (`1000` ms) a whole session's screen appearing or leaving — the
  login screen fading to black, the desktop revealing from black over it.
  `SelectionChange` (`100` ms in both themes) is the
  cross-fade as a selection mark moves between items. Reduced motion reports
  every duration as `0`, which a consumer reads as "change it now", so no
  control carries a second reduced-motion path.
- `Fonts` — one `FontSpec` (family, size, weight) per `TextRole`, referencing
  faces under `/System/Fonts`. A theme sizes text by the *job* it does
  (`Display`, `Heading`, `ItemTitle`, `WindowTitle`, `Body`, `Metric`,
  `Caption`, `SectionHeader`, `Monospace`), and every role's size is a
  percentage of one authored base size measured from the design boards, so a
  theme states one number and the whole scale follows (`AGENTS.md` §2.2).
  `Fonts::ladder` clamps that base into `MIN_BASE_SIZE_PX..=MAX_BASE_SIZE_PX`,
  so text can be neither too small to read nor too large to rasterise
  (`AGENTS.md` §5.4). `Display` is the top rung — the single dominant line on
  a screen of its own, such as the login and lock screens' clock — and is the
  one deliberate break from the tight ladder the other roles keep.
- `CursorSet` — one cursor asset id per `CursorKind`, referencing assets
  under `/System/Graphics`.

The crate owns no rendering or compositing arithmetic — that lives in the
shared rasteriser `lib/raster`. A consumer converts a theme `Rgba` into the
shared render colour at the edge (`lib/raster` provides `From<Rgba> for
tairix_raster::Color`), so the colour algebra is never duplicated
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

`Appearance` itself is `tairix_abi::desktop::Appearance`, re-exported rather
than restated: the desktop session reports the active appearance to every
application over the window channel, and the byte on that wire must be the
same value a theme carries (`AGENTS.md` §2.2). An app told the desktop has
switched simply passes it to `set_appearance`.

## Why it lives in `lib/`

Sibling userland crates may not depend on one another (`AGENTS.md` §17.4), so
the one definition the GUI crates and apps all read belongs in `lib/*`,
exactly as `lib/procinfo` is the shared home for the System Information
client helpers. The crate sits at the bottom of the §17.4 layering: its only
dependency is `lib/abi`, for the two vocabularies a theme shares with an ABI
surface (`FontWeight`, which the font service rasterises at, and
`Appearance`, which the window channel reports) — imported rather than
restated so the values cannot drift. It depends on no kernel, driver, or
userland crate.

## Stability tier

`experimental` — the surface is the Stage 7 desktop theming seam, consumed
first by `userland/gui/wm` and, in later increments, by the taskbar and the
default apps. It is `no_std` (with `alloc`) and depends only on `lib/abi`. No
`unsafe`, and no `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md`
§2.9).
