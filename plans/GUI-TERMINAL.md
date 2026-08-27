# GUI-TERMINAL — the first-class graphical terminal

Binding under `AGENTS.md`. This plan owns everything about `terminal.app` as a
*graphical* program: how large it opens, that it is **one process with many
windows** and what that costs, its icon-bar presence, the user profile behind
it, the right-click menu, the settings sheet, the colour schemes, and the
screen effects. The emulator itself — the VT parser, the grid, the pty seam —
belongs to `plans/APPWIN.md` AW4, `plans/PTY.md`, and `plans/CURSES.md`;
nothing here restates them.

Read alongside: `plans/GUI-CONTROLS-DESIGN.md` (the Reactive Alloy control
language every surface here is built from), `plans/APPWIN.md` (the window
channel), `plans/FIX-DISPLAY-ACCELERATION.md` (the accelerated layer path the
effect pipeline is shaped for).

---

## 1. Why the terminal opens at the size it does

**Status: done.**

A terminal's natural size is a character count, not a pixel count. The window
is therefore whatever the conventional 80×25 screen measures in the face
actually being drawn with, and the *text size* is what gives when a display
cannot hold it — never the grid. A terminal that quietly dropped to 60 columns
would break every program that lays itself out for 80.

The rules live in `userland/apps/terminal/src/layout.rs`:

| Rule | Definition |
|---|---|
| The grid it opens on | `COLS` × `ROWS` = 80 × 25 |
| What a grid measures | `grid_size` — the face's own advance and line height |
| What a client holds | `grid_dims` — floored, at least 1×1, capped at `MAX_DIMENSION` |
| What the furniture costs | `chrome_extent` — the shared `WindowFrame::insets`, never a second copy of the frame arithmetic |
| The size that fits | `fit_font_size` — the largest logical size ≤ the profile's whose grid plus furniture fits the screen, down to `MIN_FONT_SIZE_PX` |
| The window it asks for | `window_size` — the grid, clamped to the screen less the furniture |

The default text size is **14 logical pixels** (`profile::DEFAULT_FONT_SIZE_PX`).
On a 640×480 display at 100% the shared monospace face advances 7 physical
pixels per column at that height, so the grid is 560×350 and the framed window
562×380 — inside the screen with room left for the taskbar. A denser display
multiplies through the desktop scale; a smaller one steps the size down.

There is deliberately **no compile-time window size**, and nothing may
reintroduce one: the face's metrics come from the font service at runtime and
can differ from the compiled-in console-atlas fallback, so only a runtime
measurement is honest. Anything that needs to know where a terminal's client
sits inside its decorated window — the desktop's own pointer vertical among
them — reads `chrome_insets` rather than predicting an extent.

---

## 2. The user profile

**Status: done.**

One [`Profile`](../userland/apps/terminal/src/profile.rs) holds everything a
user can change: the colour scheme, their own custom scheme's colours, the
text size, and the strength of every screen effect. It lives in the OS
app-data store, reached through `lib/appdata`, so it is **private to this
application**: the store is gated on the bundle identity the kernel attests
and no other app the user launches can read or rewrite it
(`plans/APPDATA.md`). It was a file this app wrote under the user's home until
AD5 migrated it.

- Closed key registry (`ProfileKey`) of dotted keys in the store's shared
  `key = value` format. A save writes only what the store's layers do not
  already imply, and *Restore defaults* removes the user's opinions rather
  than freezing today's values.
- A key no layer sets means its documented default, silently. A stored value
  the registry refuses costs only itself — that one field stays at its default
  and the key is named on `stderr` — and a store the service cannot serve
  leaves the bundle's shipped defaults standing, also said on `stderr`.
- Reached through the app-data service under this bundle's kernel-attested
  identity. The app writes no file of its own and holds no new capability;
  `CAP_APPDATA_ADMIN` is the service's, never an app's.
- Colours are bare `rrggbb`, never `#rrggbb`: the grammar's comment marker
  would cut the line at the `#`.

Defaults: system scheme, 14 px text, fully opaque, every effect off — a plain,
fast terminal until the user asks for otherwise.

---

## 3. Colour schemes

**Status: done.**

`userland/apps/terminal/src/scheme.rs` is the one place a terminal colour comes
from. A `ColorScheme` is the sixteen ANSI slots plus background, foreground,
cursor, and cursor text. `Painted` resolves the scheme in force once per
repaint (never per cell) and answers every cell's foreground/background.

Shipped: **System** (follows the desktop theme, the default), **Midnight**,
**Phosphor**, **Amber**, **Ember**, **Contrast**, **Paper**, and **Custom** —
the user's own, editable in the settings sheet and persisted with the rest of
the profile. Adding a scheme is a variant plus a palette; the compiler forces
every consumer to state what it means.

The hard-coded xterm ANSI table that used to live in the renderer is now the
default scheme's palette — one definition.

---

## 4. Screen effects

**Status: done.**

`userland/apps/terminal/src/effects.rs` carries one strength per effect, each a
permille `0..=1000` a slider sets directly, and turns them into the ordered
`Pass` list a frame goes through. A zero-strength effect contributes no pass,
so a terminal with the effects off pays nothing.

| Effect | Where it happens | How |
|---|---|---|
| Translucency | While the cells are painted | The default background is filled at `background_alpha`, so the compositor's own premultiplied blend does the work and a glyph stays opaque over it. Never below `MIN_OPACITY`. |
| Backdrop blur | The compositor | Only the compositor can see behind a window, so the strength becomes a logical radius (`blur_radius_px`, capped at `MAX_BLUR_RADIUS_PX`) and is handed to the window channel's `set_backdrop_blur`. Zero when the window is opaque — a blur nobody can see is wasted work. |
| Scan lines | `Pass::ScanLines` | Dims alternate physical rows. Static. |
| Fuzz | `Pass::Fuzz` | Per-pixel luminance jitter from a cheap reproducible mixer, moving each animation step. |
| Phosphor | `Pass::Phosphor` | A decaying `Afterglow` of what was lit recently, added back in the pixel's own hue. |
| Wobble | `Pass::Wobble` | Per-row horizontal displacement along a travelling integer sine. |

**The pipeline is a description, not a pile of flags.** `Pass` carries the
*resolved physical* parameters, so a display that can composite hardware layers
can programme its own engine from the same list, with the software passes here
staying the conformance oracle for what the result must look like. That is why
the effects are a typed ordered list rather than code inlined into the
renderer.

**Animation is a clock, never a spin.** Every animated pass is a pure function
of a monotonically increasing `Phase`. The program's wait-set park carries a
one-shot frame deadline (`FRAME_INTERVAL_NS`, 20 fps) only while an animated
effect is in force; otherwise it parks indefinitely. There is no poll loop and
no periodic tick.

**A pass runs over a copy, never into the retained screen.** `render::Screen`
keeps the painted picture between frames so a repaint costs the cells that
changed (§13); a pass is a whole-frame post-process by nature — wobble
displaces rows, phosphor decays every pixel — so an active pass copies the
finished screen into a buffer, runs there, and presents whole. The retained
screen therefore stays clean and the next frame's cell diff still describes
the *text* rather than the effect's own churn, so an animated terminal
re-runs its passes without re-rendering the grid. The buffer is reused
between frames and exists only while an effect is on. Translucency and
backdrop blur are not passes, so a see-through, frosted terminal keeps the
cell-diff repaint cost.

The overlay surfaces (menu, settings sheet) are drawn **after** the effects: a
settings sheet that wobbled with the screen behind it would be unusable, and
its controls must read exactly as they do everywhere else on the desktop.

---

## 5. The right-click menu

**Status: done.**

`userland/apps/terminal/src/menu.rs`. Six typed commands built from one ordered
`Command::ALL` list and read back through the same list, so a reordering cannot
re-map a row:

| Row | Shortcut |
|---|---|
| Settings… | `Ctrl ,` |
| Larger text | `Ctrl +` |
| Smaller text | `Ctrl -` |
| Actual size | `Ctrl 0` |
| Clear screen | `Ctrl Shift K` |
| Close | `Ctrl Shift W` |

Every advertised shortcut is really honoured by `Command::accelerator`; a row
never shows a key combination that does nothing. Only combinations a shell
would not otherwise receive as a control byte are claimed, so intercepting one
never swallows input a program was waiting for.

The popup is the shared `lib/controls` `Menu`, placed by that control's own
`anchored_rect` rule, so the directory browser and the terminal share one
definition. While it is open it is modal: a press away dismisses it without
acting on what it landed on, and `Escape` dismisses from the keyboard.

It draws in the **desktop's** interface face, never the terminal's grid face.
The control resolves the theme's text role itself and accepts no typeface from
the app, so the menu reads as desktop furniture wherever it opens and its rows
neither turn monospace nor grow and shrink with the user's terminal text-size
setting. The same holds for the settings sheet below.

---

## 6. The settings sheet

**Status: done.**

`userland/apps/terminal/src/settings.rs` — a modal sheet composed from the
shared Reactive Alloy controls (`Panel`, `Tabs`, `Slider`, `Radio`, `Button`)
plus the app-local colour-well grid, on its own popup surface.

- **Appearance**: the scheme chooser, the text-size slider, and the custom
  scheme's editor — a `SwatchGrid` of the twenty editable colours with
  red/green/blue sliders for the selected well.
- **Effects**: one labelled slider per effect — opacity, backdrop blur, scan
  lines, fuzz, phosphor, wobble.
- Footer: *Restore defaults* and *Done*.

Every edit clamps through `Profile::clamp`, so the sheet can never produce an
invalid profile, and the program adopts the sheet's profile on every edit so
exactly one copy of the settings is ever live. A change re-derives the colours
and the face, re-applies the backdrop blur, reshapes the grid (the pty
follows), and writes the document.

The colour-well grid lives in the app rather than `lib/controls` because the
control library takes a control only once two independent consumers need it
(`plans/GUI-CONTROLS-DESIGN.md` §4). A second consumer moves it.

---

## 7. Compositor backdrop blur

**Status: done.**

Translucency alone composites a window straight over whatever is behind it.
Backdrop blur makes a translucent terminal read as frosted glass, and only the
compositor can do it.

- `WindowRequest::SetBackdropBlur { window_id, radius_px }` in `lib/abi`, the
  radius in logical pixels, `0` disabling, bounded by
  `WINDOW_BACKDROP_BLUR_MAX_PX`. Validated and owner-checked server-side;
  fails closed.
- `WindowClient::set_backdrop_blur` on the app side.
- The compositor blurs the back buffer inside the window's rectangle — which
  already holds everything behind it, compositing being back-to-front —
  weighted by the window's rounded-corner coverage, then blends the window
  over it. A separable two-pass box blur with running sums, so the cost is
  O(area) and not O(area × radius).
- Damage touching a blurred window widens to that window's whole rectangle, to
  a fixed point, so a change *behind* a blurred window cannot leave stale
  pixels.
- The hardware layer path cannot express a backdrop blur, so a frame
  containing a blurred window falls back to the software composite rather than
  presenting a wrong frame.

---

## 8. The window fills its frame

**Status: done.**

Two separate things used to leave dead space around the screen, and both are
gone.

- **The window manager's grab band.** A resizable window used to reserve the
  theme's `resize_grabber_extent` — 16 physical pixels at 100% — on its left,
  right and bottom, and inset the client inside it, so every resizable app
  showed a visible border of frame plate. The band is now the thin frame rim,
  the same as a fixed-size window, and the resize *hit* zone overlaps the
  client's outer `hit_slop` pixels instead: an invisible resize edge, as every
  mainstream desktop uses. Being resizable now costs an app no screen space.
  The trade-off is deliberate and documented: a press in that outer sliver
  resizes rather than reaching the app.
- **The partial cell.** A character grid can only show whole cells, so a
  freely-dragged client left up to one cell of background along the right and
  bottom that the terminal could never draw in. On a settled resize the
  terminal now snaps its client to a whole number of cells
  (`layout::snap_to_cells`) and re-maps at that size, so the frame shrinks to
  fit the grid exactly. Snapping is idempotent, so the re-map converges in one
  step and cannot oscillate.

---

## 9. The overlays are their own surfaces

**Status: done.**

The context menu and the settings sheet are **popup windows**, not pixels drawn
inside the terminal's own surface, so shrinking the terminal no longer clips
them: each opens at its own preferred size.

`WindowRequest::CreatePopup` is the protocol addition — an undecorated,
parent-anchored, app-positioned surface any app may open (offsets are relative
to the parent's client origin, because an app is never told its own window's
screen position; the session resolves and clamps the absolute placement).
A popup counts against the same per-client window budget as any other window,
is never listed on the taskbar, is kept directly above its parent every frame,
and is torn down with its parent. A refused popup is reported on `stderr` and
simply not shown — never a crash, and never a fallback to a clipped in-window
draw.

---

## 10. Why command output used to be invisible

**Status: fixed (kernel).**

Worth recording because the symptom pointed away from the cause: the terminal
opened, the prompt appeared and typing echoed, but a command's output never
arrived. Nothing was wrong with the terminal, the pty readiness, or the wake
path — the shell's *children* were being handed a closed stdout.

`apply_attach_wires` resolved an inherited standard descriptor only from the
parent's console descriptor table, never from the parent's open entries. A
pty-hosted shell's fd 0/1/2 are pty-slave *entries* and its console slots are
closed, so a spawned command inherited three closed slots: `ls` ran and every
write failed. The prompt and the echo still worked because those are the
shell's own, correctly-wired streams. An inherited base now clones the
parent's open entry behind the slot; an explicitly selected console still
cannot reach the parent's pty, so no authority is widened.

---

## 11. Screen semantics

**Status: done.**

Two screens in the tree apply the one `tairix_vt::Op` vocabulary to a
character grid — the emulator's `Grid` and the framebuffer boot console
(`lib/fbcon`) — and a program must land in the same cells on both. The
agreement is written once, as the shared `tairix_vt::conformance` script, and
each screen implements its `ScreenModel` in its own tests and runs `check`, so
altering one screen's semantics fails the other's test too. What it guarantees:

- **The wrap is owed, not taken.** Filling the last column leaves the cursor
  resting on that column with the wrap owed; the next printable glyph pays it,
  and anything that moves the cursor or erases first cancels it. Taking the
  wrap eagerly would line-feed — and on the bottom margin scroll the whole
  screen — the moment a full-screen program painted a full-width status bar,
  after which its incremental repaints land a row out. The cursor column
  therefore always addresses a real cell.
- **`DECSTBM` homes into the region**, not to the screen's top-left, so a
  program that reserves a header above its scrolling body starts inside the
  body.
- The script also pins the rubout at the right edge, erase-to-end-of-line from
  the owed wrap, tab stops, wide-glyph wrap-whole, scroll-region confinement,
  cursor clamping, the alternate screen, and save/restore.

---

## 12. A repaint costs what changed, not a window

**Status: done.**

`render::Screen` owns the window-sized premultiplied surface the grid is
drawn into and **keeps it, and the cells it was painted from, between
frames**. `Screen::paint` compares the live `Grid` against that snapshot,
redraws only the block of cells that differs, and returns it as the surface
rectangle to present. One `write_frame` — shared with the popup path —
copies just that rectangle into the shared frame region, and that rectangle
is the `DamageRect` the window channel carries, so the session converts and
the compositor recomposites the same few cells.

Before this, every present allocated a window-sized surface, re-drew every
cell in it, un-premultiplied every pixel into the frame, and declared
whole-window damage — for one keystroke. On a Pi 4B that is the ~30 ms
between pressing a key and seeing it, and it grew to ~80 ms with a
translucent background, because an alpha below 255 takes `unpremultiply`'s
three-divide path for every pixel and the compositor's blend path for every
row. The cliff is invisible: an opacity a hair under full looks opaque yet
costs the same as a fully see-through window, which is why turning
translucency "off" appeared to leave the terminal permanently slower. Scoping
the work to the damage removes the cliff rather than hiding it by snapping
the slider.

**What the diff may assume, and what it may not.** Two equal cells paint
identically *only* under the same colours and the same face, so a profile,
theme, or scale change calls `Screen::invalidate`, and so does a session
redraw request. A resize needs no such call: `present_frame` reconciles the
picture to the `DisplayMode` describing the frame region, so a surface and a
region of different shapes cannot arise however a resize half-fails, and
reshaping invalidates implicitly. The cursor block is tracked beside the
cells, so a cursor that moves damages the cell it left and the one it
entered. A damaged block is widened to whole glyphs, so clobbering a wide
glyph's continuation cell repaints its lead cell.

**Both directions are tested, not audited.** A scripted walk — typing,
cursor moves, scrolling, rendition changes, erases, cursor hide/show, and
wide-glyph clobbering — asserts after every step that every pixel the
repaint changed lies inside the rectangle it reported, *and* that the
retained surface is byte-identical to a fresh whole-window paint of the same
grid. The tight direction is pinned separately, or the suite would pass by
reporting everything: a keystroke reports exactly two cells, a cursor reveal
exactly one, and an unchanged grid reports nothing and presents nothing.

---

## 14. One process, many windows

**Status: done.**

A terminal window is not an application: the application is the emulator, and
each window is one hosted shell. `terminal.app` is therefore **one process
with a `Vec` of windows**, each carrying its own pseudo-terminal, shell child,
screen model, retained picture, `Look`, and overlay, over **one** wait-set
that carries one event mailbox for the whole process plus that window's own
shell-output and child members. Opening another window costs a pty, a spawn, a
frame region, and two wait-set members; it costs no second process, no second
event mailbox, and no second icon-bar slot. What a window costs is bounded by
the session's per-client frame budget and by this process's own stream,
process, and address-space limits — no count of its own; the last window
closing ends it.

Two things make it work:

- **Every event is demuxed on the window id it carries.** The one mailbox
  serves every window and every window's popup, so the drain resolves which
  window (or which window's overlay) an event belongs to before routing it.
  An id neither names is a window that has just closed and the event has
  nowhere to land — dropped, never guessed at.
- **Each window's wait-set tokens are minted from a monotonic slot**, not an
  index into a list that shifts, so a token names the same window for as long
  as that window lives and is never reused after it goes. A window's members
  are removed with the window.

The user's profile is the *user's*, not a window's, so a setting changed in
one window's sheet re-derives every window's look and reshapes every grid; the
sheet that made the change is the only surface re-presented on top of that.

## 15. Its icon-bar presence

**Status: done.**

The terminal declares one presence on the desktop's icon bar
(`tairix_terminal::appbar`, `plans/NEW-TASKBAR.md` T7) whose slot stands for
the emulator, not for any one window:

- **`default_action: true`** — a primary click on the slot is the terminal's
  to handle, and means *New window*. A terminal's windows are
  interchangeable, so raising one of them is less use than making another.
- **It is declared before the first window is opened.** A declared presence
  belongs to the *process*, so the declaration goes out first and the slot
  carries this menu and this default action from the moment it appears.
  Declared after a window, the session meanwhile derives a slot from that
  window alone — one that opens no menu and does nothing when clicked — so
  for as long as the gap lasts the bar shows a slot that answers nothing.
  Every application that declares a presence does it in this order.
- **The menu** follows the desktop's one icon-bar convention
  (`tairix_window::declaration`): the session-drawn *Info* row, then the
  terminal's own *New window* row, then a separator and *Quit*. The terminal
  states only its own row — it cannot place the two ends, so it cannot get the
  order wrong. Its row id lives in the `appbar` module rather than in the
  program body, because two independent readers need it: the running program,
  which matches a chosen row back to a command, and the desktop QEMU vertical,
  which reconstructs the menu to know where to click. It is derived from the
  convention's own `QUIT_ROW` so the two ids can never collide.
- **A refused declaration is an answer, not a death.** A terminal whose
  declaration the desktop refuses says so on `stderr` and carries on with no
  slot of its own; its windows are still reachable through the slot the
  session derives from them.

## 13. What remains

Nothing in the sections above. Recognised later work, none of it blocking:

- **Accelerated effects.** The `Pass` list is already the description an
  accelerated display would programme from; wiring it to `AccelLayer`
  (`plans/FIX-DISPLAY-ACCELERATION.md`) would move the per-pixel work off the
  CPU. The software path stays the oracle.
- **Scrollback.** The emulator keeps none, so the wheel has nothing to move —
  a correct, complete answer today, but a scrollback buffer would give the
  wheel and a scrollbar something to do.
- **Selection and clipboard.** There is no system clipboard yet; when one
  exists the terminal gains select/copy/paste and the menu gains its rows.
- **A profile per window.** Today one document serves every terminal window,
  and a change in one window's sheet reaches them all — which is right for a
  *user's* profile. Named profiles a user could switch a single window to
  would be a registry of documents under the same store directory.

