# PINBOARD — the desktop wallpaper, the Desktop folder, and its settings

This document is the normative specification for the TAIRiX **pinboard**: the
desktop backdrop the user sees behind every window — its wallpaper, the icons
for their own `Desktop` folder, the backdrop's menu, and the per-user
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
  and the Wallpaper pane are built from. No new control family is defined
  here.
- **Menus** — `plans/NEW-MENUS.md` owns the menu chain the backdrop menu is
  one client of (M3.2). This document states the menu's *rows and commands*
  (§7); the plate, the band, the placement, the grab and the dismissal are
  that document's, and no menu shell is defined here.
- **Settings** — `plans/NEW-DESKTOP-SETTINGS.md` owns the application the
  picture is chosen in, and DS4 owns the pane and the two served requests
  §8 describes.
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
- **Gallery** — the picture grid in the Settings application's Wallpaper
  pane, which is where the pinboard settings are edited.

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
tools/syshelp  plants /System/Graphics/Wallpapers/<Category>/    (P4)
lib/sandbox    wallpaper render ops in the desktop image service (P5)
lib/abi        pinboard_ipc: the apply rendezvous                (P6)
lib/browse     GridFlow::ColumnsFromLeading                      (P7)
userland/gui/session
               the pinboard: layer, menu, settings, service      (P8)
userland/apps/settings
               the Wallpaper pane and its served gallery         (P9)
docs           the pinboard page and every touched page          (P10)
```

Nothing about the pinboard lives in the kernel, in a driver, or in
`lib/*` that is not listed above.

---

## 2. The settings document

One document, one engine, one writer.

- **Where** — the desktop session's **published** app-data scope
  (`plans/APPDATA.md` §3.11, landed by AD10). No program spells a path to it;
  the service derives the store from the session's kernel-attested bundle
  identity, so the session is the only principal that can write it and any
  application may read what it says about its own desktop. The
  `<home>/Settings/Pinboard/pinboard.conf` path this stage originally
  specified is **deleted**, with `tairix_wallpaper::user_settings_path`.
- **Grammar** — the one `lib/appconf` `key = value` document engine.
  `lib/wallpaper` defines the closed *registry* over it and no grammar of its
  own. The registry has two readings: a **tolerant** one for the stored
  document (a value it refuses leaves that one setting at its default and is
  named, so a stale value never blanks a desktop) and a **strict** one for a
  document that arrived over the channel (a line outside the grammar, a key
  outside the registry, or a value outside a key's closed set refuses the
  whole document, because a *sender* emitting one is a defect).
- **Keys**

  | key        | value                                             | default        |
  |------------|---------------------------------------------------|----------------|
  | `wallpaper`| absolute path of the image, or `none`             | the shipped default |
  | `fit`      | `fill` \| `fit` \| `stretch` \| `centre` \| `tile`| `fill`         |
  | `backdrop` | `theme`, or bare `rrggbb`                         | `theme`        |
  | `icons`    | `leading` \| `trailing`                           | `leading`      |
  | `sort`     | `name` \| `kind` \| `size` \| `date`              | `name`         |

- **Absent is not broken.** Publishing nothing is the ordinary fresh-account
  state: the defaults above apply, silently. A store the service could not
  serve, or a value this build's registry does not accept, yields the default
  for the affected setting *plus* a ready-to-print warning line — the desktop
  comes up calm and says why, rather than guessing at a half-parsed intent or
  dying over a settings document.
- **The session is the document's only writer, by construction rather than by
  convention.** An application publishes only its own scope, so no other
  program the user launches can write this one at all. The session loads at
  bring-up and publishes on every change; the in-memory settings adopt an
  edit **only after the publish succeeded**, so memory and the store never
  diverge. Settings and the backdrop menu do not write it — they ask the
  session to (§6).

## 3. The wallpaper

- **Default set.** The masters ship read-only under
  `/System/Graphics/Wallpapers/`, filed one directory level deep in the
  **categories** `Abstract`, `City`, `Nature`, `Space`, and `TAIRiX`, and
  discovered at build time from `lib/wallpaper/assets/<Category>/` by
  `tools/syshelp` — never a hand-maintained list. A category's directory
  name *is* the label a gallery draws, so adding a category is authoring a
  directory and there is no name → label table to drift out of step.
  Discovery walks exactly one category level and fails the build closed on a
  stray file at the store root or a category name no gallery could offer
  (`tairix_wallpaper::is_wallpaper_category_name`). The default is
  `TAIRiX/tairix-dark.jpg`, named once by
  `tairix_wallpaper::{DEFAULT_WALLPAPER_CATEGORY, DEFAULT_WALLPAPER}` and
  spelled by `default_wallpaper_path()`.
- **A shipped master is authored no larger than the renderer's own maximum
  destination** (`lib/sandbox`'s `MAX_DESTINATION_WIDTH`×`MAX_DESTINATION_HEIGHT`,
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
- **The read is a streamed whole-file read, not a per-kilobyte one.** The
  session stages every wallpaper — its own backdrop and every gallery
  preview — through `tairix_rt`'s one whole-file policy (`read_fd_to_end`, 64 KiB per `fs_read`), so a multi-
  megabyte master costs on the order of a hundred syscalls rather than
  thousands. This is the load path's dominant cost on real storage, not the
  decode: a 3840×2160 JPEG decodes in tens of milliseconds, while reading it a
  kilobyte at a time cost one trap per kilobyte and, behind an SD or USB
  volume, seconds. Neither side may keep a chunk size of its own.
  - ARXFS fetches each contiguous run of such a request in **one** device
    request (`docs/src/filesystem/arxfs.md`). Both halves are needed:
    without the coalescing a 64 KiB syscall still cost ~35 device
    round-trips, which is what made the gallery take seconds behind an SD
    card.
  - **A repeat is served from RAM, and the read size must never change
    that.** Both cache layers admit by memory budget, never by request
    length (`docs/src/architecture/memory.md` §7g/§7m): a size-based
    bypass in either one silently made every run re-read the card and
    re-run the AEAD, which is what left a warm re-open of the gallery
    costing hundreds of milliseconds per master. A whole-file read of a
    hot wallpaper now costs one memory copy per 64 KiB and no device I/O
    at all.
  - **Which half is slow is measured, never inferred.** The two halves have
    unrelated causes when a gallery crawls — a cold cache or a store behind
    an SD card on one side, the sandbox pipe transfer and the decode on the
    other — and
    the decode is the one thing already known: the 26 shipped masters decode
    in 404 ms *total* at thumbnail scale and ~90 ms each full-screen, so a
    placement costing seconds is never the decoder.
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
- **The picture is never cut to.** It arrives whenever the worker finishes, so
  installing it begins a crossfade over the theme's own `BackdropChange` span:
  the arriving picture rises over the backdrop colour at login, and over the
  picture it replaces when the choice changes — margins and all, since the
  ground being left is flattened over the backdrop colour for the duration and
  released the moment the fade arrives. One `Fade` and the session's own park
  deadline, so an arrived backdrop arms no timer
  (`docs/src/desktop/session.md`).
- **Fit geometry** is one pure function in `lib/wallpaper`, shared by the
  renderer and every preview, so a preview can never disagree with the
  desktop about what a fit does.

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

Settings and the backdrop menu both **ask**; the session **decides,
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
  - **The session merges it over what it holds.** More than one surface asks
    the desktop to change and none shows every setting: the backdrop menu
    and Settings' Wallpaper pane edit the pinboard keys, its Appearance and
    Accessibility panes edit the appearance keys
    (`plans/NEW-DESKTOP-SETTINGS.md` DS3). Each renders only the keys it
    edits (`DesktopSettings::document_of`) and a key the sender did not name
    keeps the value the desktop has, so one surface cannot undo the other's
    change by staying silent about it — which taking the absent keys as their
    *defaults* would do on every apply. The merge runs on a copy, so a
    document the registry refuses leaves the desktop exactly as it was.
- **Authority** — the session serves a request only from a caller whose
  kernel-attested `Origin` carries the session's own uid; anything else is
  refused and logged. The document is display/config data, never a
  credential: it names a path, and the session then reads that path **under
  its own identity**, so an asking surface cannot use the pinboard to read
  a file it could not read itself.
- **Reply** — the shared status frame: applied, or a typed refusal. The
  identity check happens *before* the document is decoded, so an
  unattested caller cannot even reach the parser. The reply waits for the
  *store*, not for the serve loop: it is sent when the publish lands, so the
  asking surface still learns whether its document was actually written.
- **One adopt path, and it is off the loop** (`AGENTS.md` §28). A request
  adopted over IPC and a change made from the backdrop menu run through the
  very same persist-then-adopt code, so the two routes cannot diverge in what
  they write or what they redraw — and neither of them writes on the serve
  loop. The gesture *submits*; the session's settings worker publishes and
  answers with what the store then holds; the loop adopts that on the wake it
  nudges and does exactly the work the resulting change names. Persist-then-
  adopt is intact — the adopted state is always what the store said — and the
  compositor never stops for a disk. A refused publish is stated on `stderr`
  and adopts nothing.
  - A ticketed request the user's next gesture overtakes before any worker
    took it is answered right there, so no caller is left parked on an answer
    nobody will produce.
- **Reading** is not brokered: a surface reads the published document
  itself, since it is the user's own and a reader needs no coordination.

## 7. The backdrop menu

Button 2 anywhere on the backdrop opens the pinboard menu at the pointer. It is
**the seat's one menu chain** (`plans/NEW-MENUS.md`), not a surface of the
pinboard's own: the pinboard hands over a row model and the desktop's menu
service places, draws, grabs, traverses and dismisses it, exactly as it does an
application's. Its item set is closed:

| item | effect |
|------|--------|
| `Open` | activate the icon under the pointer — offered only over one |
| `New Folder` | create a uniquely-named folder in `Desktop/` and re-list |
| `Sort by …` | set `sort` (four marked items) |
| `Arrange …` | set `icons` (two marked items) |
| `Refresh` | re-list `Desktop/` now |
| `Open Desktop Folder` | open the file manager on `Desktop/` |
| `Change Background…` | open Settings at its Wallpaper pane |

`Open` resolves through the very same activation the double-click path uses,
so the two can never disagree. Managing an entry — rename, copy, delete,
properties — is deliberately absent: those verbs live in the file manager,
which owns them whole, and `Open Desktop Folder` is one row away. Offering a
half-implemented copy of them here would be the duplication the charter
forbids.

A press on empty backdrop with button 2 does not disturb the selection; a
press over an icon selects it first, so the menu always acts on what the
user pointed at. Each row's id is its command's own position in the closed set
above, so the gesture that leaves `Open` out shifts no other row's meaning. The
sort order and the arrangement in force are shown as their group's chosen
member — a bullet, disabled, with its reason stated — because choosing what
already holds is a statement of where the desktop is rather than a command.

Escape, a click elsewhere, or an activated item closes it, and the chain
clamps it wholly onto the screen, so one opened at the bottom-right corner
opens inward rather than off the edge. It obeys the service's seat rule: a
menu never appears over the lock screen or the trusted picker.
Every item the session cannot carry out (a refused `fs_mkdir`, a Settings
window that will not launch) reports why on `stderr` and leaves the desktop
unchanged — the menu never fails silently and never dies over a refusal.

## 8. Choosing a picture

The desktop picture is a **section of Settings**
(`plans/NEW-DESKTOP-SETTINGS.md` DS4), not an application beside it. There
is no wallpaper application: `userland/apps/wallpaper` is deleted, and the
backdrop menu's *Change Background…* launches `settings.app` with the
Wallpaper pane as its target.

**Settings holds no authority over the store, and gains none for this.**
Listing the shipped store needs `CAP_FS_ACCESS` and decoding an untrusted
picture needs a `CAP_PROC_SPAWN` sandbox worker. Granting either to the
application that will later carry Networking, Users and Storage is the
ambient-authority god-app that plan's §0 exists to prevent, so the gallery
is **served** rather than hosted:

- the session walks the store once at its own bring-up — `/System` is
  read-only, so the catalog is fixed for the life of the boot — through
  `catalog_categories`, `catalog_entries` and `desktop_catalog`, and answers
  `WindowRequest::QueryWallpapers` from memory, so no directory walk is ever
  on the compositing loop;
- `WindowRequest::RenderWallpaper` names a **catalog position** and a square
  side, and the session renders that candidate through its own sandboxed
  wallpaper path into a shared-memory region Settings created and granted —
  the one thing its existing `CAP_SHM` already allows. Naming a position
  rather than a path is what stops the request being used to make the
  session read a file the caller chose.

Both are of the same posture as `QueryDesktop`: seat-scoped,
capability-free, describing the caller's own desktop and granting nothing.
Both are *reads*: the only write is still the §6 apply.

The desktop renders **one preview at a time** and always prepares its own
backdrop first, so a gallery of thumbnails can neither flood the sandbox nor
make the picture the user is looking at wait. Nothing is recalled: every
accepted render answers exactly once, so a window that closes mid-render
costs one wasted decode and the slot frees itself.

A picture in effect that the catalog does not hold — one set before it was
removed from the store — is still offered and still selectable; it has no
catalog position, so its tile draws its built-in glyph and its name.

**Outstanding — offering a picture from outside the shipped store.** The
seam does not exist, so the capability is absent rather than half-built, and
both blockers are decisions for the ABI owners:

- the trusted picker's conclusion (`WindowEvent::FilePicked`) carries only a
  one-shot, owner-bound `fd_redeem` handle and **no path**, so the asking
  surface cannot learn what to write into the settings document; and
- the document names a path the session re-reads under its own identity at
  every login (§6), while the picked handle is owner-bound to the asking
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
| P8 | `userland/gui/session`: the pinboard | done (its menu is the shared chain, `plans/NEW-MENUS.md` M3.2) |
| P9 | the Wallpaper pane in `userland/apps/settings`, over the session's two served requests | done, except the picked-directory listing (§8) |
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
- **`userland/gui/session`** — the preview desk's policy (the backdrop
  taken before a thumbnail, one preview in flight, an answer freeing the
  slot, an answer to nothing dropped), and the pinboard's gestures against
  the existing
  fakes: the backdrop menu's row model (its closed row set, the marks on the
  settings in force, the id↔command inverse, the group breaks, and the rows a
  gesture leaves out shifting no id), each item's action, flow and sort changes
  re-laying the grid, an apply from a foreign uid refused, a wallpaper that
  will not decode degrading to the backdrop. The menu's *behaviour* — opening,
  clamping, choosing, dismissing — is the chain's and is tested there. The crossfade: a picture is
  invisible at the instant it is installed and stands alone once arrived, a
  frame part-way through is the mix of the two grounds (including in the
  outgoing picture's margins), the copy of the ground being left is released on
  arrival, and reduced motion arrives before it draws.
- **`userland/apps/settings`** — the gallery on the host: the candidate
  model (the "no picture" entry, a picture in effect from outside the
  catalog, which is offered but never asked for), one picture asked for at a
  time, a refusal remembered so a scale change does not retry it, a
  malformed answer refused rather than drawn, a pending tile drawing its
  placeholder, a press released away from its tile choosing nothing, and a
  choice reporting the picture without disturbing any other pinboard value.
  The pane name a launch target carries is unique, resolvable, and unknown
  names resolve to nothing.
- **QEMU** — the desktop vertical comes up with the default wallpaper drawn
  and the `Desktop` folder's icons over it.
