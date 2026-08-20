# PINBOARD — the desktop wallpaper, the Desktop folder, and its settings

This document is the normative specification for the TAIRiX **pinboard**: the
desktop backdrop the user sees behind every window — its wallpaper, the icons
for their own `Desktop` folder, the backdrop's context menu, and the per-user
settings all three read.

`AGENTS.md` is binding and wins over this document wherever they disagree.
This spec defers to its companions and MUST stay consistent with them:

- **Desktop icons** — `plans/NEW-TASKBAR.md` T16 owns the desktop icon
  surface and its grid. This document changes exactly one thing there: the
  icon flow becomes a user setting (§4) instead of a fixed trailing column.
  A desktop icon is a **shortcut** — a symlink to a bundle — not a taskbar
  pin: taskbar pinning does not exist, and applications reach the bar by
  running (`plans/NEW-TASKBAR.md` T6/T7).
- **Compositor** — `plans/COMPOSITOR-WORK.md` and `userland/gui/wm` own the
  desktop layer (`Compositor::set_desktop`). The pinboard paints into that
  one layer; it never becomes a window and never gains a second layer. It is
  repainted **per changed icon cell** (`repaint_desktop(area, …)`), never
  wholesale for a hover, a selection, or a focus change: the desktop is the
  bottom layer, so marking all of it recomposites every window above it and
  re-blurs every frosted backdrop over it
  (`plans/FIX-DESKTOP-SPEEDUP.md` D.11).
- **Icons / artwork** — `plans/ICONS.md` owns the icon asset tiers, the
  sandboxed decode, and the artwork cache. Wallpapers reuse that decode
  posture; they are *not* icons and do not enter the icon vocabulary.
- **Controls** — `plans/GUI-CONTROLS-DESIGN.md` owns every control the menu
  and the chooser are built from. No new control family is defined here.
- **Bundles / help / resolution** — `plans/APPS.md` owns the `.app` bundle
  the chooser ships as.
- **Settings stores** — the shared per-user store pattern the settings
  document follows: a bounded, fail-closed, line-grammar text document under
  the user's own `Settings/` tree, exactly as `lib/proglib`'s user overlay
  does.

## Terminology

**MUST**, **MUST NOT**, **SHOULD**, and **MAY** are implementation
requirements.

- **Pinboard** — the whole desktop backdrop: wallpaper, `Desktop` folder
  icons, and the backdrop's own gestures. It is a *layer*, never a window.
- **Wallpaper** — one raster image drawn to fill the screen behind
  everything, according to a **fit**.
- **Fit** — how a wallpaper's pixels are mapped onto the screen
  (`fill`, `fit`, `stretch`, `centre`, `tile`).
- **Backdrop** — the flat colour shown wherever the wallpaper does not
  reach, and the whole backdrop when no wallpaper is set.
- **Chooser** — `wallpaper.app`, the graphical application that edits the
  pinboard settings.

## Status

**Built.** Deliverables P1–P10 below, with the one absent capability
recorded in §8.

---

## 1. Shape of the system

```
lib/image      JPEG + PNG decode, reduced-scale decode          (P1)
lib/raster     the one image resampler                          (P2)
lib/wallpaper  settings document + catalog + fit geometry
               + the shipped default wallpaper masters          (P3)
tools/syshelp  plants the masters at /System/Graphics/Wallpapers (P4)
lib/sandbox    wallpaper render ops in the desktop image service (P5)
lib/abi        pinboard_ipc: the apply rendezvous                (P6)
lib/browse     GridFlow::ColumnsFromLeading                      (P7)
userland/gui/session
               the pinboard: layer, menu, settings, service      (P8)
userland/apps/wallpaper
               the chooser                                       (P9)
docs           the pinboard page and every touched page          (P10)
```

Nothing about the pinboard lives in the kernel, in a driver, or in
`lib/*` that is not listed above.

---

## 2. The settings document

One document, one engine, one writer.

- **Path** — `<home>/Settings/Pinboard/pinboard.conf`, spelled once by
  `tairix_wallpaper::user_settings_path`.
- **Grammar** — the shared per-user store grammar: one
  `key value` setting per line, `#` comments, blank lines ignored. The key
  registry is closed; an unknown key, a duplicate key, an over-long line, an
  over-long document, or an unparsable value refuses the whole document.
- **Keys**

  | key        | value                                             | default        |
  |------------|---------------------------------------------------|----------------|
  | `wallpaper`| absolute path of the image, or `none`             | the shipped default |
  | `fit`      | `fill` \| `fit` \| `stretch` \| `centre` \| `tile`| `fill`         |
  | `backdrop` | `theme`, or `#rrggbb`                             | `theme`        |
  | `icons`    | `leading` \| `trailing`                           | `leading`      |
  | `sort`     | `name` \| `kind` \| `size` \| `date`              | `name`         |

- **Absent is not broken.** No document is the ordinary fresh-account state:
  the defaults above apply, silently. An **unusable** document (unreadable,
  oversized, non-UTF-8, malformed) yields the defaults *plus* a ready-to-print
  warning line — the desktop comes up calm and says why, rather than guessing
  at a half-parsed intent or dying over a settings file.
- **The session is the document's only writer.** It loads at bring-up and
  rewrites the document whole on every change; the in-memory settings adopt
  an edit **only after the write succeeded**, so memory and disk never
  diverge. The chooser and the context menu do not write it — they ask the
  session to (§6).

## 3. The wallpaper

- **Default set.** Five masters ship read-only at
  `/System/Graphics/Wallpapers/`, discovered at build time from
  `lib/wallpaper/assets/` by `tools/syshelp` — never a hand-maintained list.
  The default is `tairix-dark.jpg`, named once by
  `tairix_wallpaper::DEFAULT_WALLPAPER`.
- **A shipped master is authored no larger than the renderer's own maximum
  destination** (`lib/sandbox`'s `MAX_WALLPAPER_WIDTH`×`MAX_WALLPAPER_HEIGHT`,
  3840×2160). JPEG entropy decoding cannot skip blocks: every block of the
  *source* image is Huffman-decoded regardless of the requested output
  scale, so a master far larger than any destination costs decode time no
  screen can ever use. This binds the masters this crate ships, not a
  user-picked wallpaper, which `decode_fitted`'s reduced-scale decode and
  `MAX_WALLPAPER_DECODE_PIXELS` (§5) still bound and degrade gracefully.
- **A wallpaper is untrusted input**, whether it is a shipped master or a
  file the user picked. It is read under the session's own identity, bounded
  by `MAX_WALLPAPER_BYTES`, and decoded **only** inside the parser sandbox
  (§5). A wallpaper that will not decode falls back to the backdrop colour,
  reports why on `stderr`, and is remembered as refused so a bad file costs
  one attempt, not one per frame.
- **The read is a streamed whole-file read, not a per-kilobyte one.** Both the
  session and the chooser stage a wallpaper through `tairix_rt`'s one
  whole-file policy (`read_fd_to_end`, 64 KiB per `fs_read`), so a multi-
  megabyte master costs on the order of a hundred syscalls rather than
  thousands. This is the load path's dominant cost on real storage, not the
  decode: a 3840×2160 JPEG decodes in tens of milliseconds, while reading it a
  kilobyte at a time cost one trap per kilobyte and, behind an SD or USB
  volume, seconds. Neither side may keep a chunk size of its own.
  - ARXFS fetches each contiguous run of such a request in **one** device
    request (`docs/src/filesystem/arxfs.md`). Both halves are needed:
    without the coalescing a 64 KiB syscall still cost ~35 device
    round-trips, which is what made a five-master gallery take seconds
    behind an SD card.
  - **A repeat is served from RAM, and the read size must never change
    that.** Both cache layers admit by memory budget, never by request
    length (`docs/src/architecture/memory.md` §7g/§7m): a size-based
    bypass in either one silently made every run re-read the card and
    re-run the AEAD, which is what left a warm re-open of the chooser
    costing hundreds of milliseconds per master. A whole-file read of a
    hot wallpaper now costs one memory copy per 64 KiB and no device I/O
    at all.
- **Prepared once, per (path, fit, screen).** The sandbox returns the image
  already placed at exactly the screen size; the session holds that one
  prepared surface and composites it as the desktop layer's base, over the
  backdrop colour, which is laid down first — a letterboxed or centred
  placement leaves its margins transparent on purpose, and that is what the
  backdrop is for. It is re-prepared only when the wallpaper, the fit, or the
  screen geometry changes. Nothing decodes, resamples, or parses on a frame
  path.
- **Memory.** The prepared surface is held in the shared reclaimable-memory
  model (`lib/reclaim`), so a machine under pressure drops it and re-prepares
  on demand rather than holding a screenful of pixels the user cannot see.
- **Fit geometry** is one pure function in `lib/wallpaper`, shared by the
  renderer and the chooser's preview, so a preview can never disagree with
  the desktop about what a fit does.

## 4. The icons

The desktop keeps listing the user's `Desktop` folder exactly as it does
today — the same `DirectorySource` seam, the same shared sort, the same
content-type classifier, the same `GridView`, the same double-click engine.
Two things change:

- **Flow is a setting.** `icons = leading` lays the grid out from the
  top-left, filling downward and growing a new column to the right (the
  arrangement Windows and KDE use, and the new default); `icons = trailing`
  keeps the column hugging the trailing edge. This needs one new
  `tairix_browse::GridFlow` variant, `ColumnsFromLeading` — the missing
  fourth corner of an enum that already spells its mirror image.
- **Sort is a setting** drawn from the shared `SortMode`, so the desktop and
  the file manager still agree on what "by name" means.

## 5. Decoding, in the sandbox

Wallpaper decoding joins the desktop's existing sandboxed image service
(`lib/sandbox`'s `imagerender`), rather than standing up a second worker
role: one capability-empty worker, one op space, one audit surface.

- `OP_WALLPAPER_PREPARE { screen_w, screen_h, dest_w, dest_h, fit, bytes }` —
  read the header (`tairix_image::probe`), work out what the composition can
  actually show (`tairix_wallpaper::decode_request`), decode at the smallest
  scale that covers *that* within `MAX_WALLPAPER_DECODE_PIXELS`, resolve the
  placement, hold the source with its sampled rectangle already expressed in
  the decoded image's own coordinates, and answer with the number of
  destination rows one reply frame can carry.
- `OP_WALLPAPER_BAND { first_row, rows }` — resample and place exactly those
  destination rows, and answer with their straight-alpha RGBA8 bytes.
- `OP_WALLPAPER_RELEASE` — drop the held source.

**The file's pixels reach the screen through exactly one resample.** The
placement is computed in nominal screen-model coordinates but the sampled
rectangle is mapped into the held image's own coordinates at prepare time, so
a band resamples the decoded source straight onto the destination. Resampling
twice — once to a nominal size and again into the destination — would cost a
whole intermediate image and soften the result for nothing, since the second
resample can sample the first's input directly. `Tile` is the one exception:
it repeats the source at 1:1 rather than scaling it, so the repeat is only the
right size at the nominal scale, and a decode that landed elsewhere is scaled
to it once at prepare time.

**What is decoded is what can be shown.** `decode_request` asks for the scale
at which the sampled rectangle still carries as many pixels as the rectangle
it fills — no more and no less. Asking for less would leave the resampler
enlarging pixels the file could have supplied; asking for the whole screen
when only a gallery thumbnail is being drawn would decode sixteen times the
blocks for a picture the size of a postage stamp.

Banding exists because a screenful of RGBA exceeds the sandbox's fixed
8 MiB frame bound at anything above 1080p. The bound is a defence and is
**not** raised; the transfer is chunked to respect it. Every op validates its
geometry against what `PREPARE` established, and every failure is a typed
refusal — a worker that crashes mid-band is contained, replaced, and logged
exactly as the icon path already is, and the desktop falls back to the
backdrop colour.

`MAX_WALLPAPER_DECODE_PIXELS` bounds the decoded source a wallpaper render
may hold. A screen so large that no covering scale fits the bound is served
from the largest scale that does, which costs a little sharpness and never
costs correctness or memory safety.

## 6. Applying a change

The chooser and the context menu both **ask**; the session **decides,
applies, and persists**.

- **Rendezvous** — `PINBOARD_ENDPOINT`, a reserved, seat-scoped call
  endpoint in `lib/abi/src/pinboard_ipc.rs`. Its bind is authorised by
  `CAP_IPC_BIND_PRIVILEGED` or by the caller's live seat lease, exactly as
  the notification and window rendezvous are: the session that owns the seat
  serves the pinboard shown on it, and nothing else may. The session binds
  it at bring-up and serves it from the one wait-set it already parks on, so
  the pinboard costs no extra thread and no polling.
- **Request** — `PinboardRequest::Apply { document }`, where `document` is
  the *rendered settings document* (§2), bounded and validated on the wire
  and parsed by the one engine on arrival. The wire deliberately carries no
  second encoding of the settings model: a struct of fit/flow/sort
  discriminants beside the document's own grammar would be two definitions of
  one thing.
- **Authority** — the session serves a request only from a caller whose
  kernel-attested `Origin` carries the session's own uid; anything else is
  refused and logged. The document is display/config data, never a
  credential: it names a path, and the session then reads that path **under
  its own identity**, so the chooser cannot use the pinboard to read a file
  it could not read itself.
- **Reply** — the shared status frame: applied, or a typed refusal. The
  identity check happens *before* the document is decoded, so an
  unattested caller cannot even reach the parser.
- **One adopt path.** A request adopted over IPC and a change made from the
  backdrop menu run through the very same persist-then-adopt code, so the
  two routes cannot diverge in what they write or what they redraw.
- **Reading** is not brokered: the chooser reads the document itself, since
  it is the user's own file and a reader needs no coordination.

## 7. The context menu

Button 2 anywhere on the backdrop opens the pinboard menu at the pointer,
built from the shared `tairix_controls::Menu` and presented as a popup
window the same way the taskbar's menus are. Its item set is closed:

| item | effect |
|------|--------|
| `Open` | activate the icon under the pointer — offered only over one |
| `New Folder` | create a uniquely-named folder in `Desktop/` and re-list |
| `Sort by …` | set `sort` (four marked items) |
| `Arrange …` | set `icons` (two marked items) |
| `Refresh` | re-list `Desktop/` now |
| `Open Desktop Folder` | open the file manager on `Desktop/` |
| `Change Background…` | launch the chooser |

`Open` resolves through the very same activation the double-click path uses,
so the two can never disagree. Managing an entry — rename, copy, delete,
properties — is deliberately absent: those verbs live in the file manager,
which owns them whole, and `Open Desktop Folder` is one row away. Offering a
half-implemented copy of them here would be the duplication the charter
forbids.

A press on empty backdrop with button 2 does not disturb the selection; a
press over an icon selects it first, so the menu always acts on what the
user pointed at. Escape, a click elsewhere, or an activated item closes it.
The menu is clamped to the screen, so one opened at the bottom-right corner
opens inward rather than off the edge.
Every item the session cannot carry out (a refused `fs_mkdir`, a chooser
that will not launch) reports why on `stderr` and leaves the desktop
unchanged — the menu never fails silently and never dies over a refusal.

## 8. The chooser (`wallpaper.app`)

A graphical application bundle (`kind = application`, so it installs into
the system application store and is typeable by name). It:

- lists `/System/Graphics/Wallpapers` through the shared catalog builder,
  and offers a "no wallpaper" candidate that shows the backdrop alone;
- draws a **live preview** of the selection at the top of the window, and
  every candidate as a tile in a scrolling gallery beneath it — all of them
  rendered through the same sandboxed wallpaper path, so the chooser decodes
  nothing in its own address space. A candidate the worker refuses is marked
  `unreadable` and is not asked for again, so a bad file costs one attempt;
- offers the fit, the backdrop colour, the icon arrangement, and the sort
  order as four `lib/controls` drop-downs beside the preview — the fit shown
  through the shared placement geometry in the preview, the backdrop as the
  theme default plus a fixed named palette, which also carries whatever
  colour is already in effect under its own `rrggbb` spelling so opening the
  chooser never changes it;
- applies by rendering the settings document and sending it to the session
  (§6), reporting applied / refused-with-reason / no-session beside the
  buttons rather than exiting;
- ships its own `AppInfo`, `Run`, `Help/en-US/wallpaper.md`, and its own
  icon (authored as SVG), all on disk inside the bundle.

**Pointer first.** The window is driven by the mouse; the keyboard is a
complete secondary path (Tab/Shift-Tab through the regions, arrows within
the gallery or the focused list, Enter applies, Escape closes). Clicking a
tile selects it and the preview follows; clicking a field opens its list;
the wheel, the scrollbar thumb and its track scroll the gallery. Every
interactive part is a shared control held for the life of the window, so
each owns its own hover, press and drag state, and a press released away
from the control it started on activates nothing. The gallery is
`lib/browse`'s icon-grid engine and its tiles are `IconTile`s: the view
hit-tests the pointer against the very geometry it painted, exactly as
`plans/GUI-CONTROLS-DESIGN.md` §11.34 requires of an icon view. No control,
grid or hit-test is defined in this app.

A **tile** is the wallpaper itself at tile size, always placed to fill its
square, so it answers *which* wallpaper it is; the preview panel is where
the chosen fit is shown. A fit change therefore re-renders the preview
alone, never the gallery.

**The preview is a scale model of the real screen.** The chooser asks the
session for the seat's desktop before it opens its window
(`WindowRequest::QueryDesktop`, read-only and ungated — it describes the
caller's own seat and authorises nothing), so it knows the screen's exact
extent. Inside the preview panel it draws the largest box with the
*screen's* aspect ratio that fits, centred, and renders the wallpaper into
it as the desktop would — `lib/sandbox`'s render is told the screen it
models as well as the surface it writes, and scales the source through
`tairix_wallpaper::nominal_source_size` so `Centre` and `Tile`, which are
defined in screen pixels, model correctly instead of drawing at 1:1. What
the preview shows is therefore what the desktop will show. The screen
extent is part of the preview request, so a preview rendered for one screen
can never be displayed as if it were for another, and a `DesktopChanged`
that alters the extent re-renders it.

**Outstanding — listing a user-picked directory.** Offering an image from
outside the shipped store needs a seam that does not exist yet, so the
capability is absent rather than half-built. Two things block it, and both
are decisions for the ABI owners rather than the chooser:

- the trusted picker's conclusion (`WindowEvent::FilePicked`) carries only a
  one-shot, owner-bound `fd_redeem` handle and **no path**, so the chooser
  cannot learn what to write into the settings document; and
- the document names a path the session re-reads under its own identity at
  every login (§6), while the picked handle is owner-bound to the chooser's
  task and cannot be forwarded — so even a known path would only be readable
  by the session if it can reach it itself.

`WallpaperPath` itself already accepts any absolute session-view path, so
nothing in the settings model needs to change.

## 9. Deliverables

| id | deliverable | status |
|----|-------------|--------|
| P1 | `lib/image`: baseline + progressive JPEG, reduced-scale decode | done |
| P2 | `lib/raster`: the one RGBA8 resampler, consumed by the icon and wallpaper paths | done |
| P3 | `lib/wallpaper`: settings document, catalog, fit geometry, shipped masters | done |
| P4 | `tools/syshelp`: the wallpaper graphics family | done |
| P5 | `lib/sandbox`: wallpaper render ops | done |
| P6 | `lib/abi`: `pinboard_ipc` | done |
| P7 | `lib/browse`: `GridFlow::ColumnsFromLeading` | done |
| P8 | `userland/gui/session`: the pinboard | done |
| P9 | `userland/apps/wallpaper`: the chooser | done, except the picked-directory listing (§8) |
| P10 | docs, `AGENTS.md` §3, the `plans/` jump-sheet | done |

## 10. Tests

- **`lib/image`** — hand-built baseline and progressive streams, every
  refusal path, reduced-scale selection, and a fuzz harness over both
  formats.
- **`lib/wallpaper`** — document round-trip, every parse refusal, path
  spelling, catalog filtering, and the fit geometry for every mode at
  landscape, portrait, square, and degenerate sizes. An integration test
  decodes every shipped master, so a master that the OS could not draw fails
  the build rather than the desktop.
- **`lib/raster`** — a 1:1 resample is an exact copy; a reduction weights a
  partly-covered source sample by its real coverage; an enlargement rises
  strictly rather than holding a source sample across destination pixels; an
  enlarged flat region stays exactly flat; a crop enlarges from its own edge
  samples alone; bands reassemble byte-for-byte into the whole image at every
  band height; transparent padding never bleeds its colour and an enlarged
  alpha edge keeps its colour across the ramp; single-pixel, extreme-aspect,
  and degenerate sources; and every fail-closed refusal.
- **`lib/sandbox`** — the three ops against a loopback worker: banding
  arithmetic, out-of-range bands, a band before a prepare, an oversize
  destination, and a malformed image.
- **`lib/abi`** — wire round-trip and every decode refusal.
- **`userland/gui/session`** — the pinboard's gestures against the existing
  fakes: menu open/close/act, each item's action, flow and sort changes
  re-laying the grid, an apply from a foreign uid refused, a wallpaper that
  will not decode degrading to the backdrop.
- **`userland/apps/wallpaper`** — the chooser engine on the host: the
  candidate model (the "no wallpaper" entry, a current wallpaper from
  outside the catalog, a refused thumbnail), every key's movement and the
  whole tab order, each option group including the backdrop palette and an
  in-effect colour outside it, a fit change re-rendering previews but
  remembering refusals, the rendered document matching the UI state
  exactly, each apply outcome shown, and the layout's non-overlap and
  containment at degenerate and small window sizes.
- **QEMU** — the desktop vertical comes up with the default wallpaper drawn
  and the `Desktop` folder's icons over it.
