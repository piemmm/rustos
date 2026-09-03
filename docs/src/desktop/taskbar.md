# Traditional desktop taskbar

The taskbar (`userland/gui/taskbar`, crate `tairix-taskbar`) is the
GNOME/Windows-style bar at a configured screen edge (`AGENTS.md` §10,
`PLAN.md` Stage 7, `plans/NEW-TASKBAR.md`). It floats clear of the three
screen-facing sides by the theme's `Metrics::taskbar_margin` and keeps its
normal thickness on the work-area side. This page describes the **layout,
model, and rendering**: the geometry of every region, pointer hit-testing for
input routing, the program-library popup / application-strip /
notification-area / Switchboard-tray state machines, the four **menus** it asks
the desktop to open, and painting those regions — including the clock label's **text** — into a
themed pixel surface, plus **routing** pointer, scroll, and key events into
taskbar actions and drawing **notification-icon
artwork** (scalable, themeable vector glyphs) and the desktop's **icon
artwork** (the shipped class masters and each application's own bundle
icon, read and decoded by the session and blitted here).

## Where it sits

As a `userland/gui/*` crate the taskbar depends only on `lib/*`: the shared
[`tairix-geometry`](../lib/overview.md) coordinate types, the shared
`tairix-raster` rasteriser (the premultiplied-alpha `Color`/`Surface` the
window manager also paints with), the shared `tairix-font` text rasteriser
(the built-in bitmap face and glyph blitter, `AGENTS.md` §16.4), the shared
`tairix-input` event vocabulary (the same `PointerButton`/`InputEvent`/`Key`
the window manager routes), the shared Reactive Alloy control vocabulary
(`tairix-controls`, `plans/GUI-CONTROLS-DESIGN.md` — the leading launcher
buttons and the popup's panel, search field, list rows, and scrollbar; the
[widget gallery](widgets.md) is its worked reference), the
program-library catalog engine ([`tairix-proglib`](../lib/proglib.md) — the
typed `Catalog` the popup lists), and the shared
[`tairix-theme`](theming.md) definition. It never depends on the window
manager or on any sibling userland crate, and nothing depends on it in turn
(`AGENTS.md` §17.4, §17.3) — the desktop is an optional, one-way-dependent
frontend, so a headless image simply omits it.

The taskbar holds no authority and performs no I/O: pressing a launcher or
choosing a library entry only *reports* a typed `TaskbarResponse`, and the
[session glue](session.md) — which reads the catalog stores and holds the
spawn capability — resolves and performs the action.

## Layout

A `TaskbarConfig` fixes the screen `Edge`, the bar `thickness`, and the
per-region extents. The edge fixes the bar's `Orientation`: a top/bottom bar
runs horizontally (its long *main axis* is `x`), a left/right bar runs
vertically (main axis `y`). Regions are laid out along the main axis and the
code is otherwise orientation-agnostic.

`TaskbarConfig::bottom_bar` is the house style: a **48** logical-pixel
thickness, 24 notification icons, an 80 clock, and the one `ICON_SLOT_EXTENT`
of 48 for every slot that holds a single picture — the leading launcher, each
application slot, and the trailing account capsule. That extent is named once
rather than per region, so the two ends of the bar carry the same-sized icon by
construction. Nothing sizes an icon from the thickness itself — every slot
spans the bar's content strip and each control sizes its own icon off the plate
it is given (`icon_side`), so a change to the thickness moves the icons with it
and nothing needs re-tuning.

`BarLayout::compute` insets the bar by the theme's `taskbar_margin` on the
three sides facing a screen edge: left, right, and bottom for a bottom bar;
top, bottom, and left for a left bar; and the corresponding sides for the
other edges. The margin is `5` logical pixels in both built-in themes and is
scaled through `Scale::scale_length`. The fourth side faces the work area, so
the bar keeps its thickness there. If the screen is too small for the margin,
the layout clamps it and still produces a bar.

The bar's own rim is spent before anything is placed on it: `compute` lays out
the bar's rectangle, then places every region through a placer pulled in by one
`plate_border` on both axes, so a hovered or pressed slot's plate cannot wash
over the surface's edge. `BarLayout::bar` stays the whole rectangle, rim
included, and hit-testing reads it — a press on the rim reaches the bare bar.
A bar too thin to spare two rims keeps its content rather than the inset.

From the leading end to the trailing end:

- **Library button** — the permanent leading launcher: the nine-tile-glyph
  invoker that opens the program-library popup.
- **Separator** — a one-pixel rule immediately after the Library button,
  dividing it from the application strip (see *The separator rule*).
- **Application strip** — the flexible region holding one fixed-width
  icon-only slot per *running application* in the order the session first saw
  each of them. The **leading slot** (`bar.apps[0]`) is the autostarted
  **file manager**, which runs windowless at start and opens a window on demand
  (see *The application strip*).
- **Notification area** — status/notification icons, packed immediately
  before the clock.
- **Clock** — immediately before the account capsule. Its display text is
  held by a `Clock` model whose label the caller sets (formatting a `Time64`
  value into a string is an upstream concern, `AGENTS.md` §21); the bar stores
  only the text to draw.
- **Account capsule** — anchored to the very trailing end, immovable
  (`plans/NEW-TASKBAR.md` T9). The `SwitchboardTray` model derives the shared
  `tairix-controls` `TraySignal` capsule from the Switchboard service's
  tray-signal summary plus the session's unresponsive count — one pure
  derive, dominant state hung > pressure > jobs > recovery > calm, with the
  working seam / pressure rail / recovery posture composed orthogonally, and
  an absent service deriving the calm capsule (fail closed). It wears the
  signed-in account rather than a system glyph (see *The account's identity
  disc*). Its slot is
  computed **first** among the trailing regions, so the clock, the icons, and
  the applications can never displace it — only the permanent leading launchers
  outrank it on a degenerate screen. Hover expands the capsule's instrument
  readout, presented like the other popovers
  (`Taskbar::tray_readout_layout`), and a press resolves as a tap or a hold
  (see *The Switchboard capsule's tap-or-hold gesture*).

`BarLayout::compute` turns the config plus the current application and icon
counts into the screen `Rect` of every region. All arithmetic saturates, so a
pathological screen size or extent fails closed *inside* the bar rather than
wrapping (`AGENTS.md` §2.9); a launcher, application, or icon slot that does
not fit its region is `Rect::EMPTY` and therefore never hit, and the trailing
regions clip against the permanent leading launchers (never the reverse), so
a degenerate screen shrinks the clock and icons to nothing — and, last of
all, the Switchboard capsule — rather than overlaying them on a launcher.
`BarLayout::hit_test`
maps a pointer to the `Hit` element under it (the Library button, the Files
button, an application index, a notification index, the clock, or the
Switchboard capsule), which
is what input routing dispatches (see *Input routing*).

## The separator rule

The program library is the bar's one *system* launcher; the file manager is
an application. `BarLayout::separator` states that grouping
with a single rule immediately after the Library button, so the leading end
of a horizontal bar reads

```text
[ library ] | [ files ][ app ][ app ] ...
```

and a vertical bar reads the same way top-to-bottom, the rule crossing the
bar instead of standing along it. It is one region among the others, laid out
once and read by both the painter and the tests:

- **Thickness** — one `border_thickness` along the main axis, scaled like
  every other length and floored at one physical pixel, so the rule is still
  drawn at a sub-unity scale.
- **Length** — the bar's thickness less one `control_inset` at each end, so
  the rule stops short of both long edges and never runs into the rounded
  corners the compositor applies.
- **Gutter** — the rule plus one `control_gap` on each side. Files, the
  application strip, and every trailing region begin one whole gutter past
  the Library button; the trailing clip floor moves with them, so a
  degenerate screen still collapses the clock and icons before the launchers.
- **Colour** — the theme palette's `border`, the same role every other
  separator on the desktop uses.

The rule is decoration, not a control: `hit_test` has no case for it, so a
press on the rule or in the gutter around it reaches the bare bar and is
`Ignored`. A bar too short to reach the rule, or too thin to inset it, lays
out `Rect::EMPTY` and simply draws nothing — the launchers keep their places
either way.

## The account's identity disc

The trailing capsule draws the account the session is signed in as. The
embedder names it (`DesktopShell::set_account`) and the bar keeps only the one
character it draws: a name is not an identity the bar could act on, and
holding just the mark means a live update cannot put a name on a surface that
never shows one.

The name it is given is the account's **shown** name — the human-readable name
from its record, or its login name when the record carries none
(`tairix_users::UserRecord::shown_name`) — which login exports beside `USER` as
`tairix_abi::ENV_SHOWN_NAME`. It is deliberately *not* `USER`: the login name
is what a broker is offered, and marking the capsule from it left the same
account reading `R` on the desktop and `S` on the screen the person logged in
on. The login screen's tile, the screen lock's prompt, and this capsule all
take the same string, so one account has one mark.

The picture is the account's **circular identity disc** — the shared
`tairix_icon::monogram_disc`, the same generator the login screen's account
tiles and prompt draw through, so the mark a person signed in as is the mark
they then live with. It is drawn in the theme's accent over `on_accent`, at
exactly the side it is produced for, so nothing scales or crops it. Nothing
sets a picture on an account yet; when something does, it resolves through this
same disc and stays a circle.

It is produced a `DISC_CLEARANCE` *inside* the icon square, and centred there
by the shared icon slot. Shipped artwork is authored with a little room inside
its own square — the program-library master's mark reaches about 93% of its
side — while a generated disc fills whatever square it is given, edge to edge,
so a disc at the full side reads a size larger than the launcher opposite it.
The clearance is a logical length, and the square scales too, so the two ends
of the bar keep the same proportion at every density rather than the disc
swelling as the desktop grows.

The disc is built by the paint that draws it rather than cached, so it is right
by construction at every theme, scale, and account: one round-rect fill and one
cached glyph over a few hundred pixels, against a repaint that already fills
the whole bar. A cache would be a fourth invalidation surface for a picture
that costs a fortieth of the frame it appears in, and one a theme switch could
leave stale.

Resolution is total and the disc always exists: an account name that yields no
character still marks the disc (`?`), so the capsule can never be blank. Only
where a picture of that side cannot exist at all does the capsule fall through
to `IconKind::User`'s built-in bust glyph. The disc outranks the shipped
class-artwork tier: no generic picture stands in for a person, so the capsule
asks the artwork lookup for nothing while it has a disc. Both the capsule and a
running application's slot resolve their own-picture-then-class order through
one helper, so the two cannot diverge.

## Semicircular ends

The bar is a **stadium**: its two ends are semicircles, not rounded corners.
`BarLayout::corner_radius` is therefore half the bar's own thickness — derived,
not themed, because a themed number could only ever coincide with the shape.
The window manager cuts the bar window to it through its single anti-aliased
rounded-corner path, exactly as it rounds windows, and the bar's own background
plate is laid down at that same radius so its rim follows the silhouette the
cut leaves. The shared coverage path clamps a radius to half the shorter side,
so the derived radius *is* the stadium and the two agree by construction.
Because the bar floats clear of the screen edges it faces, both ends curve
against the wallpaper. Both round through `lib/raster`'s one coverage path;
there is no second rounded-corner implementation (`AGENTS.md` §2.2). A side bar
is the same rule across its own thickness: its top and bottom are the
semicircles.

## The owned theme

The bar owns a copy of the active `Theme` (`Taskbar::theme`), adopted at
construction and swapped by `Taskbar::apply_theme`. Layout, hit-testing, and
painting all read that one copy, so the metrics a hit-test assumes and the
metrics the painter draws can never come from two different themes.

Both take `Theme::floating`, so the ground for floating chrome is adopted once
and every surface the bar paints — and every control on them — is translucent by
construction, with no control told separately and none left an opaque patch (see
*Floating chrome*).

## The per-surface repaint latch

Pixel-only state changes (a launcher hover, a popup scroll or edit, a theme
switch) set a **repaint latch**, and the embedder drains it with
`Taskbar::take_repaint` to re-present exactly when something changed — one
present per visual change, no per-frame busy repainting (`AGENTS.md` §2.16).

The latch is a `TaskbarRepaint`, and it names **each rendered surface
separately** rather than carrying one "something changed" bit:

| Flag            | Surface                                              |
| --------------- | ---------------------------------------------------- |
| `bar`           | the bar strip itself                                  |
| `library`       | the program-library popup                             |
| `picker`        | the hover window picker                                |
| `notifications` | the notification popover                              |
| `readout`       | the Switchboard capsule's expanded instrument readout |

`TaskbarRepaint::NONE` and `::ALL` are the two extremes, one constant names
each single surface, `any()` asks whether anything is pending, and `|` / `|=`
compose latches, so a mutator touching two surfaces latches both in one
expression.

**Why per surface.** The surfaces are wildly unequal in cost: measured on the
host in release, rendering the bar takes 1655 µs and the library popup 1001 µs,
against about a hundred for a small popover. With a single boolean the embedder
could not tell which had changed, so it re-rendered and re-pushed all of them
every time — a pointer drifting from one cell to the next of a small open
popover cost about 2.8 ms of rendering plus a recomposite of every window
rectangle, when a hundred microseconds and one small rectangle was the whole of
the change. That is a pointer moving over a popover, among the most frequent
interactions the desktop has, and it is why the desktop felt laggy. Naming the
surface lets the presenter repaint the cheap one and leave the others exactly as
the compositor already has them.

A menu is **not** among them: every menu is the desktop's own chain, so the bar
has no menu pixels to latch.

**The contract.** Every change that alters what a surface draws latches that
surface, and a change touching several latches all of them:

- a hover moving between bar buttons or application slots → `bar`;
- a highlight moving inside the open hover window picker → `picker`;
- opening or closing the popup → `library` **and** `bar` (the Library button
  reads as visually held open);
- raising or dismissing a notification → `notifications` **and** `bar` (the
  notification-area icon);
- a Switchboard tray summary change → `bar` when it moves the capsule's own
  glyph, state, or badge, plus `readout` while the readout is expanded and the
  reading moves anything it draws;
- a theme swap, or an edge or resize through `Taskbar::set_config` → `ALL`,
  since every surface draws from the palette and anchors off the bar's
  geometry.

Latch sites deliberately **err toward latching more, never less**: an extra
latch costs one redundant repaint, while a missing one leaves stale pixels on
screen, which is a correctness bug. Because the contract holds at every
mutator, the embedder may present *strictly* from the drained latch — and
present nothing at all when it is empty.

The tray is the one place that latch is drawn *tight* rather than generous,
and only because the tightness is proven rather than assumed. The Switchboard
service publishes a reading every couple of seconds whether or not anything
visible moved, and on a calm desktop the only thing that does move is the
readout's value line — so a generous latch repainted the full-width bar, on a
cadence, for pixels indistinguishable from the ones already there. The capsule
is gated on `TraySignal::draws_same_capsule` instead, which is exact by
construction and drift-guarded byte for byte (see
[the controls library](../lib/controls.md)); the bar is not repainted for a
figure only the expanded readout draws.

The contract holds even for a borrow the bar cannot see into. Each `&mut`
sub-model accessor latches the moment it hands the borrow out, whether or not
the caller goes on to change anything: `tasks_mut` and `clock_mut` latch
`bar`, and `library_mut` latches `library` **and** `bar`, since the same
borrow can open or close the popup and so redraw the Library button. Those
accessors carry genuine state changes — a task added, a catalog resolved, the
clock advanced — never a per-input-sample update, so the conservative latch
costs nothing on the hot path. The crate-internal routing seams the input
router borrows the popup and the menu through are the deliberate opposite:
they do not latch, because the router latches from the outcome the sub-model
reports, which is what keeps a pointer sample over the open popup from
repainting it.

That leaves exactly one thing the embedder still owns: the desktop `Scale`,
which the compositor passes per layout and render call rather than being bar
state, so a scale change is the caller's own and it knows it dirtied
everything.

## Rendering

`TaskbarRenderer::render` paints the taskbar into a `tairix-raster` `Surface`
sized to the bar using the taskbar's own theme, filling each region with a
colour role from the `Palette`. Its last argument is the caller's
`tairix_icon::IconArtwork` lookup — the session's decoded artwork, or
`NoArtwork` on a system that has none (see *Icon artwork*, below):

- the bar background is the shared floating-surface plate
  (`tairix_controls::paint_surface_plate`), the recipe every popup it opens
  already wears: a rim one `plate_border` thick in the palette's `rim`, then the
  `surface_raised` ground inside it. Both are laid down through `ground_fill` at
  the palette's `chrome_alpha`, so the blurred backdrop reads through the edge
  as well as the middle, and both are rounded by `BarLayout::corner_radius`;
- every icon on the bar is **bar-seated** (`PlateSeating::Bar`): it wears no
  perimeter in any state, and no plate at all while it has nothing of its own to
  state, so the strip reads as one bar rather than a row of boxed buttons. A
  hover raises the shared pointer wash (`surface_hover`, one clear step from the
  bar's own fill), a press compresses it (`surface_pressed`), and keyboard focus
  keeps the resting fill and draws its ring inside the slot. The rule itself
  lives in `tairix-controls` (`plans/GUI-CONTROLS-DESIGN.md` §10), not here — the
  bar chooses the seating and nothing else;
- the **Library button** is the shared `tairix-controls` `IconButton` in the
  quiet `Neutral` role, carrying the shipped `Library` artwork over its
  nine-tile glyph — compressed while its popup is open, washed under the
  pointer;
- the **separator** is filled in the palette's `border` colour,
  through a plain `Surface` fill — the renderer draws the laid-out
  `BarLayout::separator` rectangle and knows nothing of the gutter
  arithmetic behind it (`AGENTS.md` §2.2);
- each **application slot** is one shared `tairix-controls` `TaskbarItem` —
  the bar's application buttons have exactly one visual recipe
  (`AGENTS.md` §2.2), and that recipe is icon-only: a centred icon sized off
  the plate, no label, in a slot the same extent as a launcher's, so a run of
  applications reads as one strip of equal icons. The application's label is
  model data (`AppSlot::label`) a context surface reads, never ink on the bar,
  and a slot carries **no** presence, focus, or minimised mark: it rests as a
  bare glyph on the bar and washes lighter under the pointer, and nothing
  else;
- each slot's per-application **artwork** (the owning bundle's icon, read and
  rasterised by the session through the sandboxed icon pipeline — see
  [the session](session.md)) is blitted through the control in place of the
  built-in glyph. It comes from the bundle the kernel attested owns the
  process, so an application is recognised by its own picture;
- each **picker cell** is one shared `tairix-controls` `WindowPreview`: a
  captioned thumbnail of that window's last presented frame, scaled by the
  session to the cell's own thumbnail rectangle, falling back to the
  application's class glyph when a window has no frame yet;
- each notification icon slot draws a **scalable vector glyph** (see
  *Notification icons* below), tinted in the **muted** foreground colour;
- the **Switchboard capsule** is the shared `tairix-controls` `TraySignal`
  drawn in its slot — its shipped `switchboard` artwork (the built-in mixer
  glyph where the system ships none) with its live badge, seam, rail, and
  beads, bar-seated like every other icon on the strip, so it carries no
  outline of its own — and `TaskbarRenderer::render_tray_readout` paints the
  expanded instrument readout as its own popover surface, rounded by the window
  manager with `TrayReadoutLayout::corner_radius`.

On top of those plates, the renderer draws **text** with the shared `tairix-font`
`BitmapFont` (the built-in Inconsolata EX + M PLUS 1 Code + D2Coding + Noto Sans
Hebrew family): the clock label, centred in the clock region, is the only text
on the bar — an application slot draws its icon alone, so nothing can spill into
a neighbouring slot. Glyphs are composited through `tairix-raster`'s one
premultiplied-alpha `over` path — no blitter or colour
algebra is duplicated here (`AGENTS.md` §2.2).

The window manager presents the surface and cuts it to
`BarLayout::corner_radius` through its single anti-aliased rounded-corner path,
exactly as it rounds windows; the bar's own background plate is laid down at
that same radius, so its rim curves with the cut instead of squaring off across
it (`AGENTS.md` §2.2). Region rectangles are screen-space; each is translated
into the bar's
local surface space, the translation saturates, and `fill_rect` clips, so a
degenerate layout paints nothing rather than panicking (`AGENTS.md` §2.9).
Switching themes simply re-renders with the new palette.

`TaskbarRenderer` is a small stateful object — the region fills and the clock
are cheap to repaint every frame, but the vector notification glyphs are not,
so it holds a `tairix_reclaim::ReclaimCache` of rasterised
glyphs across frames, built by `icon_cache` from the shared
`tairix_reclaim::desktop::disposable_ui_cache` policy: owned by the seat,
bounded by a budget derived from the real framebuffer byte size, dropped
under memory pressure, and wiped on release. The renderer is the right home
for that state: the `Taskbar` model stays pure data. `render_library` (the
popup painter) needs no cache of its own, so it stays a `&self` method.

## Floating chrome

The bar and every popup it opens — the program-library panel, the hover window
picker, the notification popover, and the Switchboard readout — are drawn with
the floating theme the session derived once for all of its chrome
(`DesktopSession::floating_theme`, which also grounds every menu plate), and the
session asks the compositor for the theme's `chrome_backdrop_blur` — `7` logical
pixels in both built-in themes — behind each surface. Along-bar popup placement is clamped to the bar's own span, so
no popup enters the wallpaper gap.

Every *background* on those surfaces therefore lets the backdrop through: the
hover and press wash under a bar icon, the library's rows and its search field,
the readout's *Open Switchboard* button, a notification card, and the scrollbar
channel. Each keeps the colour role it wears when solid — the bar, its context
menu and the readout ground in `surface_raised`, a `Panel` in `surface` — so a
resting row still matches its panel and a hover wash still steps away from it.
What a surface is there to *show* stays solid: icons, labels, a control's rim,
focus rings, role fills, a menu's highlighted command, and the pressure rails
and beads, because each has to read against whatever wallpaper is behind it. A
*surface's* own rim is not one of those marks but its edge, so it takes the
ground's weight — a step lighter than the ground on the dark theme, a step
darker on the light one. Each surface draws one translucent layer, so a floating
panel has no separate header band.

## Icon artwork

Every application icon the bar draws resolves through **one** rule, written
once in `render` rather than restated per slot (`AGENTS.md` §2.2):

1. the artwork the slot carries of its *own* (a running application's own
   bundle icon, a library row's own icon, a picker cell's window thumbnail, the
   trailing capsule's account disc), else
2. the artwork the `IconArtwork` lookup holds for the slot's `IconKind` — the
   shipped class master under `/System/Graphics/Icons`, else
3. the control's built-in vector glyph.

The bar reads no file and decodes no image: it *asks* the lookup the session
owns, at exactly the pixel side the slot will be drawn at (`app_icon_side`,
`picker_thumbnail_size`, and the controls' own `icon_side`), so nothing is
ever rescaled at draw time. Because the third rung always exists, resolution is
**total**: a lookup that
answers nothing — `tairix_icon::NoArtwork`, a machine with no
`/System/Graphics`, or a cache that is giving memory back under pressure —
still renders every element, so a headless-graphics or freshly-installed
system stays fully usable rather than showing blank slots (`AGENTS.md` §10,
§2.9).

## Notification icons

The notification area holds an ordered list of status icons, each with a
stable `IconId` and a theme **asset id**. When the bar renders, every
notification slot resolves its asset id to a `tairix-icon` `IconKind`
(`IconKind::for_asset`, falling back to a generic glyph for an unknown id,
`AGENTS.md` §2.9), builds the matching scalable `VectorIcon` in the **muted**
foreground colour, rasterises it to the slot size at the active scale, and
composites it onto the bar through `tairix-raster`'s `Surface::blit`. The
glyph is artwork, not a flood fill, so the raised bar background shows through
around it. The icons rasterise through the *same* polygon path
(`Surface::fill_polygon`) the cursors use — there is no second scan converter
(`AGENTS.md` §2.2) — and a slot too small to hold a glyph paints nothing
rather than panicking (`AGENTS.md` §2.9).

Rasterising a glyph is the expensive step, so the `TaskbarRenderer` does it
only once per tint and size: its `ReclaimCache` is keyed by `IconKind` within a
`(tint, pixel-size, set-generation)` epoch (`IconEpoch`), so repeated frames
reuse the cached glyph and only a theme change (new tint), a scale change (new
size), or an installed icon set (new generation) re-rasterises — the
SVG-first "convert once, re-render only on a scale or theme change" rule
(`AGENTS.md` §10), sharing the same disposable-UI-cache policy the window
manager uses for cursors (`AGENTS.md` §2.2).
See [SVG asset decoding](svg-assets.md) for the caching layer and
[Desktop icons](icons.md) for the vector representation and the glyph set.

## The program-library popup

`LibraryPopup` is the folder-organised application launcher the Library
button opens (`plans/NEW-TASKBAR.md` T5). It is a pure model over the
**resolved** program-library `Catalog` the session hands it
(`LibraryPopup::set_catalog`) — the machine store merged with the user's
overlay through `tairix_proglib::merge` (see
[the program library](../lib/proglib.md)). The popup never touches the VFS;
choosing an entry only reports `LibraryLaunch { entry }` with the entry's
identifier, and the session resolves the bundle and launches it through the
ordinary signature-checked load gate.

The surface is composed from the shared Reactive Alloy vocabulary
(`plans/GUI-CONTROLS-DESIGN.md`): a `Panel` whose anchor notch points back at
the Library button, a `SearchField` filter, one shared `ListRow` per folder
or entry, and a `ScrollBar` when the rows overflow the viewport. Folders are
the closed ten-folder taxonomy in its canonical order; a folder with no
entries is never listed; entries sort by display name within their folder. An
empty library renders a calm "No programs are catalogued" placeholder — and a
filter matching nothing "No matching programs" — never an error
(`plans/NEW-TASKBAR.md`).

Opening is deterministic: the search comes up cleared, every folder expanded,
the cursor and scroll at the top, and the keyboard on the search field, so
the same catalog always presents the same way.

### Geometry

`Taskbar::library_layout` computes the popup's geometry (`LibraryLayout`):
the panel opens *outward* from the bar — above a bottom bar, below a top bar,
to the inner side of a left/right bar — aligned to the Library button and
clamped along the bar's own span, so it cannot enter the wallpaper gap. Its
height is sized to the rows it has, capped by the space between the bar and
the opposite screen edge; overflowing rows scroll.
The panel chrome overhead is *measured* by probing the shared `Panel`
geometry rather than re-deriving its arithmetic, so a metrics change can
never drift the layout from what the panel draws (`AGENTS.md` §2.2). Widths,
row heights, and the corner radius are *logical* lengths converted through
the one shared `Scale::scale_length`; all arithmetic saturates, and a screen
too small for even one row yields chrome with an empty viewport rather than a
panic (`AGENTS.md` §2.9).

### Keyboard model

While open the popup is modal and fully keyboard-driven
(`plans/GUI-CONTROLS-DESIGN.md` §9): `Tab` cycles the two focus fields
(search ↔ rows); `Up`/`Down` move the row cursor (wrapping), `Home`/`End`
jump, `PageUp`/`PageDown` move by a viewport, and the view follows the
cursor; `Enter` (or space) activates the cursor row — a folder toggles its
expansion, an entry launches; `Left` collapses a folder or climbs from an
entry to its folder, `Right` expands or steps into the first entry; typing
anywhere routes into the search field (type-to-filter, case-insensitive,
flat name-sorted results), `Enter` in the search launches the first match,
and `Escape` clears a non-empty search, then dismisses. Everything fails
closed: activating with no cursor, launching with no match, and stepping in
an empty list all change nothing (`AGENTS.md` §2.9).

### Rendering

`TaskbarRenderer::render_library` paints the open popup: the `Panel` chrome
(anchored back at the Library button), the search field, the visible rows —
a folder row carries the open/closed folder glyph, its label, and a trailing
entry count; an entry row is indented beneath its folder and draws **the
application's own icon** (below) over the app-bundle glyph; the hovered row
raises its fill, and the keyboard cursor row shows the shared selection rail
and focus ring — the calm placeholder when nothing is listed, and the
scrollbar when the rows overflow. Like the bar, the window manager places it
and rounds it with `LibraryLayout::corner_radius`, and it returns `None` while
the popup is closed (`AGENTS.md` §2.9).

### Each application's own icon

An entry row shows the icon its bundle ships, and the popup carries that
artwork the same way the application strip does: the owner resolves it, the bar only
draws it (`AGENTS.md` §2.2, §17.4). Three methods express the split:

- `LibraryPopup::visible_icon_requests(layout, scale, theme)` reports one
  `LibraryIconRequest { row, side, entry }` per entry row **the viewport
  actually shows**, each carrying the exact pixel side that row's slot will
  draw at. A hundred-entry library therefore costs the session a read and a
  decode per *visible* row, never per catalogued application, and scrolling
  asks only for the rows that just came into view.
- `set_row_artwork(row, artwork)` files an answer; an out-of-range index is
  ignored rather than mis-filed.
- `row_artwork(row)` is what `render_library` blits.

Any rebuild of the row list (a new catalog, a changed filter, a folder folded
or expanded) clears the filed artwork, so a stale index can never draw one
application's icon on another's row. A row with no artwork draws the
app-bundle glyph, which is why a library still lists legibly on a system with
no artwork at all.

## The window registry

`TaskList` holds one `TaskEntry` per top-level window: its id, its title, and
whether it is minimised. At most one window is *focused*, and the list also
remembers the *previous* window — the one focused immediately before the last
handover to a different one, the MRU-of-two behind the Switchboard capsule's
middle-click switch.

The bar draws none of it. A slot on the bar is an **application**, and a
window is reached through the hover picker; this is the one registry those two
read, together with the capsule's scroll-to-cycle and previous-task gestures.
Focusing a window restores it, which is what choosing its picker cell does;
minimising is the title bar's own control and the only way a window leaves the
screen without closing. Adding a window with a duplicate id, or
removing/retitling/minimising an unknown one, changes nothing and is reported
as such — the window manager assigns unique ids, so a clash signals a bug
rather than a benign retry.

## The application strip

`AppStrip` holds the session's resolved view of each running application
(`plans/NEW-TASKBAR.md`). The session hands the bar one `AppSlot` per
application — its display label and class glyph, its rasterised bundle
artwork, the windows it owns (by id, never a second copy of their state), the
menu it declared over the window channel, and whether it handles the slot's
primary click — through `Taskbar::set_apps`. `Taskbar::app_icon_side` exposes
the exact pixel side a slot's icon paints at, through the same control
geometry the renderer uses, so the session rasterises artwork at exactly the
drawn size.

**A slot carries no presence or focus mark.** The bar shows which
applications are running by showing them at all; there is no running bar
under a slot, no focus seam, and no recessed minimised plate. Only the
pointer's own wash distinguishes one slot from another.

**Which applications hold a slot** is the session's answer, not the bar's
(see [the session's icon bar](session.md#the-icon-bar)): an application that
declared a presence keeps its slot for the life of its process, windows or
not, and one that declared nothing but owns a window still gets a slot — with
no menu and a click the session answers by raising — so no window is ever
unreachable. A bundle whose *signed* manifest sets `APPINFO_FLAG_NO_ICON_BAR`
has no slot either way: it is one the desktop already reaches another way, so
a slot would be a second route to the same window.

Because those two cases differ, an application declares its presence
**before** it opens its first window. A declared presence belongs to the
process, so declaring it first is what makes the slot carry the
application's menu and click behaviour from the moment it appears. Declared
after a window, the session meanwhile derives a slot from that window alone,
and for as long as the gap lasts the bar shows a slot that opens no menu.

**An application on the bar outlives its windows.** Closing the last one puts
it away rather than ending it: the slot stays, and *Quit* on that slot's menu
is what ends the process. So a slot must be able to produce a window, which
is what the declaration's `AppBarClick` says:

- `AppBarClick::Open` — every click is the application's. The session relays
  an `AppBarDefault` event and does nothing else, one click one actor. (The
  terminal opens a fresh window; the file-manager component opens one at the
  user's home.)
- `AppBarClick::RaiseOrOpen` — the session raises and focuses the most
  recently used window, and with none relays the click so the application can
  bring one back. This is what an ordinary single-window application declares.
- `AppBarClick::Raise` — raise the most recently used window and, with none,
  nothing. Only for an application that ends with its window and so can never
  be clicked windowless: the Date & Time app runs under an elevated account,
  and a resident windowless instance would keep that authority alive behind an
  empty slot.

A press reports `AppDefault { app }`, `AppRaise { app }`, or `Ignored`
accordingly.

The old click-to-minimise toggle is deliberately gone: a slot is an
application, not a window. Minimising lives on the title bar, and a minimised
window comes back by being chosen in the hover picker — or, for an application
with no slot at all, by cycling the task list from the Switchboard capsule.

## The hover window picker

`WindowPicker` is the surface that chooses between one application's windows.
Resting the pointer on a slot whose application owns at least
`PICKER_MIN_WINDOWS` (two) windows reports `ShowWindowPicker { app }`; the
session — which owns the windows' pixels — answers with one `PickerEntry` per
window through `Taskbar::show_window_picker`, and the bar lays out a grid of
captioned thumbnail cells opening outward from the slot, clamped onto the
screen.

**It opens only above one window.** With a single window there is nothing to
choose, and the slot's own click already reaches it, so sweeping the pointer
along a bar of ordinary single-window applications pops nothing up.

### Both edges are timed, and the clock resolves them

- It opens once the pointer has **rested** on the slot for
  `PICKER_OPEN_DELAY_NS` (one second). A pointer crossing the bar on its way
  somewhere else has asked for nothing, so the dwell is what separates a hover
  from a passing pointer.
- It closes `PICKER_CLOSE_GRACE_NS` (a fifth of a second) after the pointer
  comes to rest on **neither** the slot nor the panel. The panel hangs a gap
  away from the bar, so a pointer travelling from the slot to a cell
  necessarily leaves the bar's surfaces on the way; closing on that crossing
  would make choosing a window impossible. Reaching the panel — or coming back
  to the slot, which is what happens when a window merely passes over the bar
  — cancels the grace outright.

Neither edge is polled or slept on. `TaskbarInput::park_deadline_ns` folds the
pending transition into the wait the desktop was going to make anyway, and
`TaskbarInput::tick` resolves it when that wait expires; an idle bar arms no
timer at all. A dwell whose slot moved under it (the strip was re-pushed while
it ran) opens nothing rather than showing the windows of whichever application
now holds that index.

### The grid, and why no cell is unreachable

Cells wrap into as many columns as the space beside the bar holds and as many
rows as follow, and a grid with more rows than that space shows **scrolls**:
`PickerLayout` carries the shared `tairix-controls` `ScrollBar`'s gutter, the
wheel over the panel walks the grid, and pressing or dragging the bar moves it
like any other scrollbar. A cell outside the visible rows is `Rect::EMPTY` and
can never be hit — and scrolling brings it into view, so an application with
far more windows than fit across the screen still has every one of them
selectable. The first visible row is clamped to the grid at layout time, so a
density change under a scrolled panel cannot leave it showing no cells at all.

### Thumbnails are prepared, never scaled in one go

Scaling a window's frame reads every one of its pixels, so building a whole
picker at once would stop the desktop's serve loop for as long as that took.
The session instead scales **one window per turn of its loop** while the dwell
runs (`DesktopShell::advance_window_thumbnails`, reported as owed by
`window_thumbnails_owed`), so the picker opens already drawn and the loop stays
free to serve input, presents, and IPC between the slices. A cell whose
thumbnail has not landed draws that *application's* glyph rather than a hole,
and `Taskbar::set_picker_thumbnail` fills it in when a later slice arrives.
Nothing is retained past the hover: the pointer leaving drops the prepared
pixels.

Otherwise it is a pointer surface and nothing else. A press on a cell reports
`WindowChosen { id }` (the session raises and focuses that window, which also
restores it if it was minimised); a press on the plate's own chrome is claimed
and does nothing; it takes no keyboard, so the focused window keeps its keys;
and it closes when the slot is clicked, or when the application stops having a
choice to offer.

## The bar's menus

The bar draws **no menu**. Every menu on the desktop is the seat's one chain,
drawn by the session ([menus](./menus.md)), so a secondary press on the bar
answers `TaskbarResponse::OpenMenu(MenuRequest)` — which menu it is, the rows to
draw, and where the plate hangs — and the desktop opens it. The bar keeps no
menu state at all: while a chain is up the seat's grab means no event reaches
the bar, so there is nothing for it to be modal about.

Four subjects ask for one:

- **A running application's slot**, offering the menu that *application*
  declared over the window channel — and nothing at all when it declared none,
  so the bar never invents one on an application's behalf. Every declared row
  is stated in declaration order with the enablement, mark, accelerator
  caption, disabled-row reason and role it asked for; a declared separator opens
  the group its next row begins rather than becoming a choosable row; a declared
  `Submenu` row's own rows become its child plate. Choosing a row answers
  `AppMenuChosen { app, item }`, which the session relays back to the declaring
  process — the bar never interprets an id. The plate is titled from the
  bundle's **signed** manifest, so a menu cannot be titled as an application it
  is not.

  The one row inside such a menu that is the desktop's own is the
  **information** row (`AppMenuRow::Info`, drawn as *Info*): its child is the
  application's information panel, a `FactList` of the bundle's *signed*
  manifest — name, version, and its purpose and author when the manifest states
  them. An application declares only that the row exists, so it cannot state an
  identity that is not its own inside system-drawn chrome, and an omitted field
  is absent rather than a blank row.
- **A program-library entry row** in the open popup: the two things the popup
  can do to a row that its own click cannot. *Open* answers `LibraryLaunch`;
  *Create Desktop Shortcut* answers `CreateDesktopShortcut`, which the session
  turns into a symbolic link in the user's own `Desktop` folder — named after the
  entry, pointing at its bundle directory (`plans/SYMLINKS.md`). Both rows come
  from one `EntryRow` list that the model is built from and the answer is read
  back through, so a reordering cannot silently re-map what a row does;
  `EntryRow::label` is the one definition an embedder's test or a QEMU pointer
  script aims by. Either row closes the popup: it is modal, so leaving it up
  would stand between the user and what they asked for.
- **The Switchboard capsule**: the desktop's system quick actions (see *The
  system quick-actions menu*).
- **The clock**: the reading the bar is drawing, and setting the machine's time
  (see *The clock's menu*).

A menu opens outward from the bar's own edge, anchored at the slot or row it is
about; the chain bounds and flips it to stay on screen. The bar performs nothing
(`AGENTS.md` §5.4): a chosen row is read back through the same table the plate
was built from into the typed response the session carries out, and a row's id
is its *command's* own position in that table rather than its position on the
plate — so a menu that leaves a row out (the system menu without *Switch
User…*) shifts no other row's meaning.

## Input routing

### The bar acts on the pointer only while it holds it

The bar knows where its own regions are. It cannot know whether anything is
*drawn over* them: a window dragged across the bar leaves the clock at the
clock's coordinates, and a router that hit-tested that position alone would
light up, open popovers, and act under a window the user is working in.
Stacking belongs to the desktop's input seat
([the session](session.md#routing-one-seats-input-to-the-taskbar-and-the-window-manager)),
which resolves which surface the pointer rests on and hands the pointer events
to that one router. Every event this router is handed is therefore its own to
act on.

`TaskbarInput::set_pointer_focus` is the other half of the contract, and it
takes a `tairix_input::PointerFocus`:

- **`Left`** drops every hover the bar is drawing and starts the hover window
  picker's closing grace. It cannot be inferred from a position, because the
  pointer usually has not moved — a window was raised over the bar, or a drag
  took the pointer — and testing that unchanged position would answer "still on
  the clock", leaving a highlighted slot lit over someone else's window. The
  panel goes on the grace rather than at once because leaving the bar's
  surfaces is also what a pointer travelling *to* a cell does; the clock ends
  it, so nothing is left stranded.
- **`Entered { at }`** adopts the position the pointer arrived at and refreshes
  the hover there. The pointer can arrive without moving (the window above the
  bar closed), and no motion event exists for that. An arrival back onto the
  surfaces an open picker lives on **cancels** its closing grace: a window
  passing over the bar is not the pointer leaving, so the panel must not go
  down behind it. An arrival deliberately opens **no** hover surface and arms
  no dwell: a window closing is not a gesture, and the next real motion,
  rested out, opens the picker if the pointer is still on the slot.

A delivered `PointerMoved` says the same thing as an `Entered` and carries the
same position, so a router handed one is already entered.

### What each press does

`TaskbarInput` is the taskbar's input router, the counterpart of the window
manager's `InputRouter`. It consumes the **same** shared `tairix-input`
`InputEvent` stream the compositor routes (`AGENTS.md` §17.4, §2.2), tracking
the pointer position from motion events (which also drives the leading
buttons' and application slots' hover feedback through the repaint latch,
and arms or drops the hover window picker's timed edges). With the popup and
menu closed
it acts only on a primary or secondary press, hit-tested against the current
`BarLayout` and reported as a `TaskbarResponse`:

- a primary press on the **Library button** opens the program-library popup
  (`OpenLibrary`);
- a primary press on an **application slot** performs that application's
  declared click behaviour, or raises its most recently used window (see *The
  application strip*). The leading slot — the autostarted **file manager** —
  resolves idempotently: a press raises its window forward rather than
  launching a second copy;
- a **secondary** press on an application slot opens the menu that
  application declared;
- a primary press on an open **picker cell** chooses that window
  (`WindowChosen { id }`); one on its grid's scrollbar belongs to the
  scrollbar (a thumb grab reports no offset of its own until the drag moves,
  so the panel stays up under the hand that grabbed it); one on the picker's
  own plate is claimed and closes it;
- a primary press on a **notification icon** reports its `IconId`
  (`NotificationActivated`);
- a primary press on the **clock** is claimed and inert — the clock is a
  reading, not a control — while a **secondary** press opens the clock's own
  menu (see *The clock's menu*), which asks for nothing by itself;
- a primary press on the **Switchboard capsule** resolves as a tap or a
  hold (see *The Switchboard capsule's tap-or-hold gesture*), a press
  inside the open readout drives its one safe action or is claimed inert
  like the notification popover's chrome, **scrolling** over the capsule or
  its readout cycles the running tasks (wrapping both ways), and a
  **middle** press over the capsule switches to the previous task (the task
  list's MRU-of-two) — each failing closed when there is nothing to cycle
  or return to.

Any other button, a release, a key, or a press that misses every region
changes nothing and is reported as `Ignored` (fail closed, `AGENTS.md` §2.9).

While a **menu** is up the bar sees no input at all: the chain holds the seat's
pointer and keyboard, so motion, presses and keys all route into it and none
reaches the bar ([menus](./menus.md#the-grab)). The hover picker is *not* modal:
it claims a press over its own plate and nothing else.

While the popup is **open** the router treats it as modal and consumes the
whole event stream — presses, releases, scroll, and keys all route into the
popup, so a click lands on exactly one thing (`AGENTS.md` §2.1):

- a primary press on a **folder header** toggles it at once; a primary press
  on an **entry row** arms the click instead, and the release that ends the
  press launches the entry (`LibraryLaunch { entry }`, closing the popup) —
  unless the pointer left the row it pressed, in which case nothing is
  launched, so a press-and-move-away never launches the wrong thing;
- a primary press on the **Library button** toggles the popup shut
  (`LibraryDismissed`);
- a press **anywhere else** — any button — dismisses the popup without
  acting on what it landed on (`LibraryDismissed`), the standard click-away
  behaviour;
- scroll wheels the row viewport; keys drive the keyboard model above.

Popup-internal changes (a hover, a scroll, an edit, a fold) are reported as
`Ignored` with the repaint latch set, so the embedder re-presents without
mistaking them for actions.

## The Switchboard capsule's tap-or-hold gesture

The capsule is the desktop's way into the Switchboard overview window
(`plans/NEW-TASKBAR.md` T11), and one primary press resolves into exactly
one of two destinations, reported as
`OpenSwitchboard { section }`:

- **tap** — a press and quick release opens the running-task section
  (`CommandSection::Tasks`, the wire section the bar names directly), the
  panel's NOW column;
- **hold** — a press held past `LONG_PRESS_AFTER_NS` (half a second) opens
  the **Recovery** section instead, where a hung application's recovery
  actions live;
- the open readout's one safe action, **"Open Switchboard"**, reports the
  same response as a tap, so the pointer and the readout reach one
  destination through one route (`AGENTS.md` §2.2).

The session performs it: it asks the Switchboard service to open — or,
when the service has died, revive and open — its window at that section.
The press *is* the demand, so there is no respawn loop
(`plans/NEW-TASKBAR.md` T10).

The threshold is resolved against the monotonic time the caller passes to
`TaskbarInput::handle`, on whichever event the router next handles once it
has elapsed — ordinarily a motion sample taken while the press is still
held, or the release when none arrives sooner. Nothing polls and nothing
sleeps (`AGENTS.md` §2.23). One gesture reports exactly one response: a
hold that has already opened Recovery never also opens Tasks on release,
and a press dragged off the capsule before release opens nothing at all
(fail closed, `AGENTS.md` §5.4).

## The system quick-actions menu

A **secondary** press on the Switchboard capsule asks the desktop for the
system menu, drawn by the seat's one chain like every other
(`plans/NEW-TASKBAR.md` T13). Its rows are one table, `system::ROWS`, from
which both the stated rows and the row → command mapping are derived, so the
two can never disagree (`AGENTS.md` §2.2):

| Row | What the session does |
|---|---|
| About This System | opens the Switchboard's overview |
| System Monitor | opens the Switchboard's task list |
| Task Shell | launches the terminal bundle |
| Light / Dark Appearance | switches the desktop's theme |
| Lock Screen | secures the screen behind this user's password |
| Log Out | ends the session; the login supervisor re-prompts |
| Restart / Shut Down | confirmed, then relayed to the one holder of the power capability |

The bar holds none of this authority. Each row reports a typed response and
the session resolves it, and every row whose backing is missing renders
**non-actionable with its reason stated** rather than being hidden or
silently offered: an uninstalled terminal bundle, a Switchboard that has not
attested it can power the machine, or a session that has attested it cannot
prompt for a password. Every such attestation defaults to refusing, so a bar
that was never told offers nothing it cannot deliver (`AGENTS.md` §5.4).

Locking heads the last group because it is the one way out of the session
that *keeps* the session; everything below it ends work in progress. The row
is offered only where the session runs on a console whose login supervisor
can re-verify the signed-in user (`DesktopShell::set_elevation_available`) — a
lock that could never be undone is a trap, not a security measure. What the
lock then guarantees is described in [the desktop
session](session.md#the-screen-lock).

## The clock's menu

A **secondary** press on the clock asks the desktop for the clock's menu
(`plans/NEW-TASKBAR.md` T17) — the button that asks for a menu everywhere else on
the desktop, here too. A
primary press is claimed and inert, like a status signal's: the clock is a
live reading rather than a control, and a left click that pops a menu up is a
menu nobody asked for. Its rows are one table, `clock_menu::ROWS`, from which
both the rendered menu and the row → command mapping are derived:

| Row | What it is |
|---|---|
| the reading the bar is drawing | a statement, not a command: it cannot be chosen |
| Set Date & Time… | asks the session for an account that may set the clock |

The heading repeats the label the bar is *already* drawing rather than
re-deriving a time of its own, so the menu and the bar beside it can never
disagree. When no wall-clock time has been established this boot the bar draws
`clock::UNSET_LABEL` (`--:--`) — the clock's menu is where a time is set, so
it must stay visible and keep its width — and the heading
states *Time not set* rather than repeating dashes or a fabricated `00:00`.

Setting a clock needs `CAP_TIME_SET`, which the bar does not hold and a
desktop session must never hold. The row therefore reports a typed response
and the session asks for an account that does hold it (see [the desktop
session](session.md#asking-for-an-account-that-may)). Whether the row can act
at all is the same attestation the *Lock Screen* row reads — one console
fact, `set_elevation_available`, since both need a re-authentication broker —
and while it is absent the row renders non-actionable with its reason stated
and emits nothing.

## Theming

The taskbar owns the active theme (see *The owned theme*) and adopts a new
one with `Taskbar::apply_theme`; the rest of its state is untouched, so a
runtime dark/light switch needs no model relayout (`AGENTS.md` §10). The
region **colours**, the control plates, and the text **foreground** roles are
wired through that theme by the renderer. The interactive light/dark switch
is the appearance pair in the system quick-actions menu above; the session
also switches programmatically (`DesktopSession::set_theme`).

## Tests

The crate's headless unit tests cover edge/orientation, the task-list
focus/minimise rule, notification add/remove deduplication, the region layout
and hit-testing for a bottom bar (both permanent launchers included),
vertical-bar layout, all four edges, overflow clipping, degenerate
(tiny-screen) fail-closed clipping of the launcher buttons, DPI scaling of
layout and hit-testing, and the theme-driven corner radius. The repaint tests
pin the per-surface attribution itself: a hover that changes only a bar button
latches `bar` alone, a highlight moving inside the open menu latches `menu`
alone, a pointer move that changes no hover state latches nothing at all,
opening and closing the popup latches `library` and `bar`, raising and
dismissing a notification latches `notifications` and `bar`, typing in the
popup's filter latches `library` and not `menu`, a theme swap and a
`set_config` edge move each latch every surface, and draining clears the
latch. Further tests pin the borrow rule: each `&mut` sub-model accessor
latches its surfaces even when the caller changes nothing, reading through the
immutable accessors latches nothing, a repeated pointer sample over the open
popup latches nothing, and `NONE` / `ALL` / `any()` / `|` compose as
documented.
The application-strip tests cover the strip's span from the launchers to the
trailing group, slot hit-testing on every edge, a slot's square matching a
launcher's at more than one scale, degenerate fail-closed clipping, the
accessors the session pushes through, a stale hover clamped away by a fresh
strip, and the absence of any presence or focus mark. The declared-menu tests
cover the exact rows an application's declaration draws (order, marks,
accelerator captions, disabled rows and the reason each states, a destructive
row's role, the separator folded into the next row's group break, the
submenu's own child excluded from the top level, the bar's own *Info* row),
a menu at the format row cap, a disabled row that cannot be chosen, the
relayed row ids (including from inside a one-level submenu), `Escape` stepping
out of an open child before dismissing, an application that declared no menu
opening nothing at all, and the information panel: the manifest-attested facts
in order, an omitted purpose or author absent rather than blank, a pointer over
the panel claimed and inert, and the panel disappearing with the menu that
carried it. The click tests cover the whole matrix — each of the
three declared `AppBarClick` behaviours with and without a window, so the
resident-application case cannot drift from the two that were always there —
and that a second click never minimises. The picker tests cover the refusal below two windows, the
open at two, the cell layout on every edge with a clipped cell that can never
be hit, the highlight latching the picker alone, a cell choice raising (and
restoring) that window, a press on the plate's own chrome, the absence of a
keyboard path, closing on pointer departure and on a slot click, the close
that comes with losing the second window, survival of a strip update that
keeps the choice, the refusal to open under a modal surface, and the rendered
cells (a window's own frame, and the application's glyph where a window has
none).
The popup model tests cover taxonomy-ordered folders with name-sorted
entries, hidden empty folders, folder labels, the calm empty and no-match
placeholders, the deterministic reopen state, case-insensitive filtering with
Enter-launches-first-match, Escape's clear-then-dismiss, wrap-around cursor
movement, folder fold/expand from both pointer and keyboard, focus cycling
with type-to-filter, and cursor-follows-view scrolling. The input tests cover
both buttons' presses, click-away dismissal that activates nothing beneath,
secondary-press dismissal, wheel scrolling of an overflowing popup, and hover
repaint latching. The row-click tests cover the whole gesture: a press that
barely moves is still a click and launches, a release away from the row that
was pressed launches nothing (nor the row the pointer ended over), a rebuild
under a held press launches nothing at all, and a folder header acts on the
press and arms nothing.
The rendering tests probe painted pixels for the bar regions; the bar's own rim
(the theme's rim tone at the chrome weight on both built-in themes, the ground
one `plate_border` further in, lighter than that ground on the dark theme and
darker on the light one, still see-through, one scaled border thick at 100% and
200%, and cut away at the rounded corner); both launchers
resting bare on the bar (glyph ink on the bar's own fill, with no role fill,
no rim of their own, and no reactive edge); a hovered or pressed slot inking
inside itself and never over the bar's rim, on both themes; the hover wash
appearing under the pointer in that slot alone and never as an edge; the
Library button compressed while
its popup is open; notification
glyphs (including the unknown-asset fallback and cache retint on theme
switch); the clock's text; an application slot's centred icon with no label
ink anywhere beside it; and
the popup's panel, rows, hover/selection states, placeholder ink, scrollbar,
and dark / light /
high-contrast rendering.
The icon-artwork tests drive `render` with a recording lookup: the
Library launcher button asks for its kind at its drawn side
and blits what comes back, an application slot with no artwork of its own
falls back to its kind's artwork before the glyph, two running applications
each draw their own picture and only their own (one the session could not
attribute keeping the shared glyph), the popup asks only for
the entry rows the viewport shows (and, after a scroll, only for the rows
that just appeared), a rebuild drops stale row artwork, and a bar rendered
through `NoArtwork` still draws every element from its built-in glyphs — the
property that keeps a machine with no `/System/Graphics` fully usable.
The Switchboard tray tests cover the slot's trailing-most placement on every
edge (and its survival order on degenerate screens), the summary→state derive
matrix (absent service, calm top-task preview, jobs, every pressure kind,
recovery, hung, and compositions), repaint latching, hover readout geometry
on every edge and its survival across a live update, the tap-or-hold gesture
(tap → Tasks, hold → Recovery on the first sample past the threshold and on a
still-fingered release, never twice for one press, nothing at all once the
press drags off, and no gesture armed by a press elsewhere on the bar), the
readout's "Open Switchboard" action and its inert press claim away from it,
scroll cycling and the middle-click previous-task switch with their
fail-closed cases, and pixel probes of the capsule, rail, seam, and badge
tones across themes.

Those tests all run the model headless. What only a real machine can show
is that the bar is wired to the running applications around it, and
`tests/integration/appbar_qemu_aarch64` proves that on the aarch64 `virt`
board: it boots the graphical session, opens the program library, launches
the terminal from its row, right-clicks the slot the session gave that
process, and chooses *New window* from the menu the **application itself**
declared. Its coordinates are not hand-copied — the host script drives this
very crate's `TaskbarInput` with the events the guest will receive, reads the
bar's own rectangles back out of `Taskbar::layout`, and drives the production
menu chain over the model the press asked for to find the row's rectangle —
and it passes only when the terminal's bundle is loaded, its first window is
created and painted, and a *second* create follows: nothing else the script
injects opens a window, and the desktop's own surfaces are session-painted
compositor windows that never call the window channel, so that second window
can only be the chosen row reaching the application. Three scan-out readbacks
check the screen: the bar before anything runs, the slot carrying the
application's glyph with its window up, and both windows with the one
application still holding exactly one slot.
