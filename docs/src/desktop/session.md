# Desktop session glue

`userland/gui/session` (`tairix-desktop-session`) is the desktop's **session
glue** (`AGENTS.md` §10, `PLAN.md` Stage 7): the component that owns the shared
theme registry and the taskbar model, loads and merges the program-library
catalog the taskbar's popup lists, and applies or surfaces the taskbar's typed
responses — the work the bar itself deliberately cannot do.

## Why a separate component

The taskbar deliberately owns no theme registry, no filesystem reach, and no
spawn capability. Its buttons and its program-library popup only *report*
typed `TaskbarResponse`s — open the library, open the file manager, launch
this catalog entry, this task was activated. Acting on those belongs to the
session layer, which holds the shared `tairix_theme::ThemeRegistry`, reads
the catalog stores under its own kernel-attested identity, and (in the `Run`
binary) holds the window-manager and process capabilities.

It composes the other GUI crates and `lib/*` only — `tairix-taskbar`,
`tairix-proglib`, and the shared `tairix-theme` definition — which is the
permitted `userland/gui/*` edge (`AGENTS.md` §17.4). Nothing outside
`userland/gui/*` depends on it (§17.3), so a headless image omits it cleanly.

## The program library

The popup lists the **resolved** program library (`plans/NEW-TASKBAR.md` T5):
the machine-wide store (`/System/Settings/ProgramLibrary/library.conf`)
merged with the logged-in user's overlay
(`<home>/Settings/ProgramLibrary/library.conf`) through the one
`tairix_proglib::merge` (see [the program library](../lib/proglib.md)).
Reading those documents needs a filesystem capability, so it is the
session's job — the `library` module's `load_library` reads both stores
through the `SessionFileReader` seam, parses them with the one fail-closed
catalog engine, and merges them:

- an **absent** store is the ordinary fresh-installation state — an empty
  catalog, no complaint;
- an unreadable, oversized, non-UTF-8, or malformed store contributes an
  **empty catalog plus a ready-to-print warning line** — the desktop
  degrades to a calm empty library and says why on `stderr`, rather than
  guessing at a half-parsed store or dying over a settings file
  (`AGENTS.md` §2.24, §5.4).

The `Run` binary loads the library at session bring-up and **re-reads the
stores each time the popup opens**, so an edit made through `applib` shows
without restarting the session. The merged catalog is handed to the popup
with `DesktopShell::set_library`, which re-presents an open popup in the
same frame. A `LibraryLaunch { entry }` response is resolved back through
that same catalog — the entry's bundle names its `Run` binary — and spawned
asynchronously under the session's own identity, with a refusal reported
loudly and non-fatally (see *Launch bookkeeping*).

Both things a row can ask for resolve through the **one** lookup,
`library::catalogued`, so a row can never launch one bundle and link another,
and a catalog that changed under the click is refused once, in one wording.

### Desktop shortcuts

`CreateDesktopShortcut { entry }` — the entry menu's second row
(`plans/SYMLINKS.md` S5) — is a symbolic link the session creates in the
user's own `Desktop` folder under its own identity. The desktop owns the
naming, because it owns the folder: `Desktop::shortcut_to` takes the entry's
**display name** as the link name and its `BundlePath` as the target, stored
verbatim.

- The target is the bundle **directory**, which is what makes the shortcut
  read as an application: `lib/browse` decides bundle-ness from the target's
  own leaf name, never from the link's.
- A display name is not automatically a file name — the library permits a `/`
  and a `:` in one — so it is validated through the one shared
  `tairix_path::validate_file_name`, and a refusal carries that rule's own
  reason.
- **A name already taken is the kernel's answer.** `fs_symlink` replaces
  nothing, so a collision is `AlreadyExists` at create time, reported as the
  refusal it is. Picking a free name instead would silently make a second,
  differently-named shortcut for a user who already has one, off a listing
  the rate-limited re-list may have left stale.
- The create runs target-first through the same `settle_desktop_create` tail
  the new-folder create uses, so both state a refusal identically and both
  show the fresh name by re-listing. A desktop never dies over a shortcut it
  could not make (`AGENTS.md` §2.24).

## The icon bar

The bar's middle is one slot per *running application*
(`plans/NEW-TASKBAR.md`), and deciding which applications hold a slot — and
what each one's slot offers — is the session's job. The `apps` module's
`AppBarService` is that job.

An application here is one **kernel-attested process**, and two facts put one
on the bar, either alone being enough:

- it **declared** an icon-bar presence over the window channel
  (`WindowRequest::SetAppBar`, relayed to the service through the
  `AppBarBridge` seam the window host borrows). A declaration keeps the slot
  for the life of the process, windows or not — so *Quit* stays meaningful
  and "open a fresh window" stays reachable. Re-declaring replaces the
  previous declaration whole, which is how an application changes a row's
  enablement or its mark.
- it **owns a window**, which gives it a slot even with no declaration, so no
  window is ever unreachable. Such a slot has no menu, and its click is one
  the session answers by raising: it invents neither on an application's
  behalf.

Slots keep the order the session first saw each process in, so the strip never
reshuffles under the pointer. A process leaves the bar when it has neither a
declaration nor a window left — which, for a declaring application, is when
the window engine proved the process gone and withdrew its declaration.
`MAX_BAR_APPS` bounds the strip, so a process that declares from every fork it
makes is refused rather than admitted to a slot nothing will draw.

### A bundle may present no slot at all

A bundle whose **signed** manifest sets `APPINFO_FLAG_NO_ICON_BAR`
(`icon-bar = false` in its `AppInfo.toml`) is dropped from the strip whichever
of the two facts above put it there. Two ship that way, both because the
desktop already reaches them another route, so a slot would be a duplicate:
the **Switchboard**, which the bar's own permanent trailing capsule stands
for, and the **wallpaper chooser**, which the backdrop menu's *Change
Background* row opens.

The claim lives in the manifest rather than on the window channel because a
*running process* must not be able to hide itself from the bar: the manifest
is signed, so a bundle opts out and a program cannot. `AppBarService::strip`
reads it from the same per-bundle record it already reads the slot's identity
from, so it costs no extra manifest read, and it keeps that record for a
dropped bundle too — evicting it would re-read the manifest on every wake.
`AppBarService::is_iconless` tells the embedder which processes were dropped,
so a window absent from the strip by design does not read as a strip gone
stale.

Such a window is still an ordinary entry in the task list, so a minimised one
is reached by cycling that list from the Switchboard capsule.

### An application on the bar outlives its windows

Closing the last window puts a declaring application away rather than ending
it; *Quit* on its slot is what ends the process. A slot must therefore be able
to produce a window, which is what the declaration's `AppBarClick` states —
`Open` for every click, `RaiseOrOpen` to be asked only when there is nothing
to raise, or `Raise` for an application that ends with its window and so can
never be clicked windowless. The bar resolves that into `AppDefault` /
`AppRaise` / nothing; the session only relays.

### Identity is the manifest's, never the process's

A slot's label, icon, and information-panel facts come from the **signed**
`AppInfo` of the bundle the desktop launched that process from — resolved from
the existing launch table and the window engine's attested owner records,
never from anything an application sent. So an application cannot state an
identity that is not its own inside system-drawn chrome
(`AGENTS.md` §23.1). The manifest is read once per bundle and remembered while
an application from it is on the bar, so a second copy of one application
costs a lookup rather than a read.

A process the desktop did **not** launch — a shell-spawned program — has no
bundle to attest, so its slot carries a neutral label and no version, purpose,
or author at all: the panel states what it read and never what it did not.

`Taskbar::app_icon_side` exposes the exact pixel side a slot's icon paints at,
so the session rasterises artwork at the drawn size.

### Relaying the bar's outcomes

Three taskbar responses reach the declaring process or its windows:

- `AppDefault { app }` → the session delivers `WindowEvent::AppBarDefault`
  through `WindowServer::deliver_app_event`, addressed to the route the
  *declaration* recorded — never to anything the event carries — so a bar
  event can only reach a process that asked to be on the bar.
- `AppMenuChosen { app, item }` → the same path, carrying
  `WindowEvent::AppBarMenu { item }`. The id is the application's own; the
  session never interprets one.
- `AppRaise { app }` → no application is told anything: the session raises and
  focuses that application's most recently used window (the window registry's
  own answer — the focused window when this application owns it, else the one
  it handed focus to last, else the newest it opened).

A refused delivery tears the owner's windows down exactly as a refused
window-scoped send does.

### The hover picker's thumbnails

A `ShowWindowPicker { app }` response is answered with one cell per window,
built by `picker_cells`: each carries that window's **last presented frame** —
the compositor's own copy of pixels the session already holds, so no new
authority is involved — scaled to the cell through `Surface::resampled`, the
one resampler on the desktop (`AGENTS.md` §2.2). Both surfaces are
premultiplied, so the scale is one allocation and one filter pass: no
straight-alpha round trip, and no copy of the whole frame.

**Scaling is sliced, because a frame's every pixel is read.** Building a
screenful of thumbnails in one turn of the serve loop would stop the desktop
for as long as it took, so `DesktopShell::advance_window_thumbnails` scales
**one** window per turn while the pointer rests out the picker's opening dwell,
and `window_thumbnails_owed` shortens the loop's park to nothing while a slice
remains (the wait still reports a ready member first, so slicing never starves
input). The picker therefore opens already drawn, and a slice that lands after
it opened fills its cell in place. A window whose pixels were released under
memory pressure, or that has not presented yet, simply has no thumbnail and its
cell draws the application's glyph. Nothing is retained past the hover: the
pointer leaving drops the prepared pixels, so thumbnails cost memory only while
there is a picker to show them in.

### Icon artwork

Bundle icon bytes (SVG or PNG) are **untrusted third-party input**, so the
session never decodes them in its own address space. Instead, they go to the
**parser-sandbox icon-rasterisation service**: the session's own binary
re-entered as a capability-empty worker ([the sandbox
page](../security/sandbox.md)). The rasterised RGBA pixels are verified and
cached per `(asset path, pixel side)`, including refusals; a missing or bad
icon falls back to the shipped application-bundle artwork and then to the
shared application-class glyph.

### One artwork store for the whole desktop

The application slots are not the only thing on the bar with an icon, so the
read, the sandboxed decode, and the cache are **not** the icon bar's private
machinery: they are the shared two-tier artwork layer (`lib/icon`'s
`ArtworkCache` plus its `ArtworkReader` / `ArtworkRasteriser` seams), and the
`DesktopShell` owns exactly one of each for the seat (`AGENTS.md` §2.2).

- `DesktopShell::set_artwork_resolver(resolver)` installs the live
  `ArtworkResolver` — on a running system the desktop's icon-decoder thread
  (below), or `InlineArtwork` over the VFS reader and the sandbox worker where
  the kernel granted no thread; in tests an inline pair of fakes. A shell that
  is never given one starts with a resolver that finds and decodes nothing, so
  a bare shell draws built-in glyphs rather than failing.
- `DesktopShell::artwork_parts` hands out the cache and the resolver together,
  which is how the embedder resolves the strip's slots without borrowing the
  shell twice.
- The same cache answers the bar's `IconArtwork` lookup (through
  `IconArtworkSource`) for the shipped class masters under
  `/System/Graphics/Icons`, so the launcher buttons, an application slot, a
  picker cell, and a library row all draw out of one store and one budget.

Every lookup is keyed by (what was resolved, pixel side), and a refusal is cached
like a success, so an application whose icon will not decode costs one read
and one sandbox round trip — not one per frame.

### The decode happens on a worker thread

A read plus a sandbox round trip is far too much to spend inside a paint: a
launcher opening on thirty applications used to pay it thirty times, on the
serve loop, before its first pixel reached the screen. The decode therefore
runs on the session's **icon-decoder thread**, and the paint that misses draws
the built-in glyph for that frame.

- The policy is `tairix_icon::ArtworkDesk`: what has been asked for, what the
  worker is producing, what has come back, and what has already been answered.
  It holds no lock, no thread, and no syscall, so every rule it carries is a
  host test. It lives in `lib/icon` beside the `ArtworkResolver` contract it
  implements, because the file manager drives the same desk from its own
  reader thread (`plans/NEW-FILEMANAGER.md`) and `userland/apps/*` may not
  depend on `userland/gui/*`. The `Run` binary adds the runtime's futex mutex, a
  condition variable the worker parks on (never a spin), and the shared wake
  pipe the session's wait-set already watches.
- The worker owns its **own** sandbox child, exactly as the wallpaper worker
  does, so no sandbox handle ever crosses a thread. It decodes through
  `tairix_icon::render_artwork` — the same function `InlineArtwork` calls — so
  which thread ran it cannot change what it produced.
- It nudges the session when its queue drains rather than after each icon, so a
  bring-up wanting thirty of them costs one repaint and they appear together.
- A **declined** answer stops a landing chasing its own tail. The artwork cache
  is budgeted, so it can be asked to hold more than it will; without a rule, a
  decode it refused to retain would be asked for again by the very repaint the
  landing drove. The cache reports the refusal and the desk holds that key until
  the pressure band moves. An answer the cache *took* is simply forgotten by the
  desk, so an icon it later evicts is decoded again rather than drawing its
  glyph for ever.
- Most surfaces adopt a landing simply by asking the cache again. The two that
  *store* the picture instead — the bar's application strip and a window's
  title-bar identity — are offered it again explicitly on the wake.
- **Asked for early, not on the frame that needs it.** A decode off the loop
  still costs a round trip, so a surface that first asks as it *paints* shows a
  screenful of glyphs and fills in one icon at a time afterwards. The desktop
  therefore warms what it knows it will draw, the moment it knows it:
  `DesktopShell::warm_icon_artwork` on every catalog read and strip re-resolve
  (the launcher popup's first screenful of rows, named by
  `Taskbar::catalog_icon_wants` from the same layout the paint uses),
  and `DesktopShell::warm_launched_artwork` for every application in the launch
  table at the two sides a window of it wears — a spawn, a load, and the app's
  own bring-up before there is a window to put it on. Both go through
  `ArtworkCache::prefetch`, so an icon already held asks for nothing and a
  session with no decoder thread pays a lookup per icon and no more.
- A kernel that grants no thread, or a wake pipe it refuses, leaves the decode
  on the serve loop through `InlineArtwork`: slower under load, never wrong,
  and stated once on `stderr`.

### The library popup's per-row icons

A program-library row shows its own application's icon through exactly the
resolution above, driven off what the popup says it is showing:
`resolve_library_icons` asks the popup for its
[visible icon requests](taskbar.md#each-applications-own-icon), resolves each
row's bundle icon at that row's own pixel side, and files the answer back.
A row whose entry declares no icon — or whose asset is absent, over the read
bound, or undecodable — falls back to the shipped `AppBundle` artwork, and
then to the row's built-in glyph, so a row is never blank.

It runs at the top of `DesktopShell::present`, before the paint, so a row
that has just scrolled into view is drawn with its icon in the same frame; a
row that already holds right-sized artwork is skipped, and a closed popup
resolves nothing. Filing artwork deliberately does **not** latch a repaint:
latching from inside the pre-paint resolution would ask for another present
every frame, forever.

## The desktop icon surface

`Desktop<S: DirectorySource>` (`userland/gui/session::desktop`) is the user's
own `Desktop` folder shown as a column of icons down the screen edge their
own settings name, drawn into the window manager's own [desktop layer](wm.md#the-desktop-layer)
— beneath every window and reachable through no window id. It is a
*directory view*, not a new kind of surface: it lists the folder through the
same `DirectorySource` seam the trusted file picker uses, orders the listing
with the shared `sort_entries`, classifies each child with the shared
content-type registry (see [File-type classification](apps.md#file-type-classification--the-one-content-type-registry)),
and lays its tiles out with the shared `GridView` under
`GridFlow::ColumnsFromLeading` or `ColumnsFromTrailing` — the file manager's
own grid flows rows from the leading edge; the desktop's column hugs one
vertical edge and grows a new column inward as it fills, but both share one
cell geometry and one hit-test (`lib/browse::layout`). It paints through the
same public `grid_tile` /
`grid_metrics` helpers and the shell's own icon-artwork lookup, so a folder
shows the shipped folder artwork, a file its content-class artwork, and an
application bundle the icon it carries in its own `Resources/`, falling back
to built-in glyphs exactly as the file manager's grid does. Each icon is a
shared `lib/controls` `IconTile` — the picture over its name with no plate of
its own, so the icons sit on the wallpaper rather than in a row of boxes, and
only a hovered, selected, or focused icon paints anything behind itself. A
selected icon is filled with the theme's accent at half opacity, its edge
softened, so the wallpaper still reads through the mark.

The one place the desktop's geometry deliberately differs from the file
manager's is what it does with the space a column has left over: the field
takes `GridFill::FixedPitch`, keeping one tile-plus-gap pitch from the edge its
icons hug. It is a fixed field rather than resizable content, so an icon stays
where the user last saw it whatever the work area's exact extent is — it does
not drift when the taskbar's band or the display mode changes by a few pixels.
The file manager's resizable grid takes `Spread` instead and shares a row's
leftover width out between its tiles (see [Rendering](apps.md#rendering)).
Neither ever lays out a tile an edge would cut.

**Pointer and keyboard.** A primary press selects the icon under it (or
clears the selection on an empty desktop) and arms the shared
`DoubleClickTracker`, so a second press within its window activates the icon
— the desktop can never disagree with the file manager about what a gesture
means. Motion drives hover feedback and, on arrival from elsewhere, the
gesture-driven re-list below. A secondary press opens the [pinboard's context
menu](#the-pinboards-context-menu). While the desktop holds the keyboard, the
arrows move the selection (down/up one icon, left/right one whole column),
`Enter` activates it, and `Escape` clears it. *Which* horizontal arrow moves
later into the listing follows the live arrangement, because columns grow
rightward from the leading edge and leftward from the trailing one — the
selection therefore always moves the way the icons the user can see actually
run.

**Activation** resolves by entry kind: a directory opens the file manager
*at that path* (passed as the program's own first argument, which the file
manager now honours); an application bundle launches directly; a plain file
resolves its association through the catalog the session holds and launches
that application with the file as its argument; and a file nothing is
associated with is refused, stating the reason on the error stream rather
than failing silently. Every launch rides the session's existing
[asynchronous launch path](#launch-bookkeeping), so the compositor never
blocks on one.

**Re-listing is gesture-driven, never timed.** There is no
filesystem-change notification in this system, so the desktop re-lists at
bring-up, after a session action that could have touched the folder, and on
pointer arrival from elsewhere — rate-limited by `RELIST_MIN_INTERVAL_NS` so
sweeping the pointer on and off the desktop cannot turn a gesture into a
re-listing loop. There is deliberately **no timer and no polling loop**: a
periodically-waking desktop would keep a core busy to discover nothing
(`AGENTS.md` §2.23). A re-list that actually changed the folder also
refreshes the library catalog and the file associations
(`DesktopOutcome::relisted`), so an application installed after bring-up is
picked up without a restart.

### The pinboard settings live on the desktop model

`Desktop<S>` owns the user's `tairix_wallpaper::PinboardSettings` — the
wallpaper and its fit, the backdrop colour, the icon arrangement, and the sort
order (`plans/PINBOARD.md` §2) — as the **single** copy inside the session:
`DesktopShell` reads them back out of the desktop (`Desktop::settings`) rather
than holding a second set that could drift from the one the icons are actually
laid out by. A desktop starts on the defaults an absent store document implies,
so it is fully specified before the embedder has read anything.

An edit arrives through `Desktop::apply_settings`, which reports what the edit
asks for instead of making the caller guess: `None` means the settings were
already in force and there is nothing to do at all, and otherwise the layer must
be repainted — that is what a change *is* — while the returned `PinboardChange`
names the further work on top of it (`relayout` when the arrangement moved,
`relist` when the sort order changed, `wallpaper` when the image or its fit
changed). Changing the sort order therefore never decodes a wallpaper, changing
the wallpaper never re-reads the folder, and a new backdrop colour costs one
repaint.

The settings' own vocabulary is deliberately *not* the file browser's:
`IconSort` is bridged to `lib/browse`'s `SortMode` by one small function in
`desktop.rs`, and `IconFlow` to `GridFlow` by another, so the listing still runs
through the single shared `sort_entries` and the single shared `GridView` while
the settings store stays free of the browser engine's dependency weight.

### The desktop layer: wallpaper or backdrop, then icons

`DesktopShell::present_desktop_area` repaints the layer **in place**, through
`Compositor::repaint_desktop`, into the screen-sized buffer the compositor
already holds: a hover, a moved selection, or a re-list repaints the desktop
often, and a whole screen of pixels is not something to re-allocate per frame.
The layer is opaque and covers the screen. Its base is the wallpaper the
embedder prepares and installs with `DesktopShell::set_wallpaper` (it holds the
capability to read the user's chosen image and the sandbox that decodes it, and
it fits the pixels through the one shared placement in `lib/wallpaper`); the
shell blits what it is handed and parses nothing. The backdrop colour the
settings name — the active theme's own desktop colour for `Backdrop::Theme`, the
chosen flat colour for `Backdrop::Colour` — is laid down **first**, under the
wallpaper, so a letterboxed or centred picture shows the user's own backdrop in
the margins it does not cover (the sandbox leaves those pixels transparent on
purpose) rather than whatever the previous frame left there. The icons are then
drawn over that base, in the work area, so nothing is ever drawn under the
taskbar. A layer the heap will not give back leaves the desktop exactly as it
was rather than blanking it.

#### The backdrop dissolves; it is never cut to

The whole screen changing between two frames is the one change on a desktop
nobody can miss, so every ground change crossfades over the theme's own
`MotionInteraction::BackdropChange` span (`600` ms): the wallpaper appearing at
login once the worker has read and fitted it, one wallpaper giving way to
another when the choice changes, and a wallpaper giving way back to the plain
colour. `BackdropFade` (the `fade` module, beside the session's screen fade)
holds it, `set_wallpaper` begins it, and the loop steps it in `animate` — so it
shares the screen fade's timing, its park deadline, and its reduced-motion
answer, where a zero duration means the ground is simply there.

Which ground is being *left* is the whole of the arithmetic, and both cases come
out as the straight mix of the two grounds, so no frame part-way through shows a
colour neither ground has:

- Leaving the plain colour, the layer's own base fill **is** that ground, so the
  arriving picture landing at the fade's strength (`Surface::blit_faded`) is
  already the mix. Nothing is allocated to fade a picture in over a colour,
  which is the login case.
- Leaving a picture, the layer is painted whole and the ground being left is laid
  back over it at the inverse strength. That ground is *flattened* — the outgoing
  picture composited over the backdrop colour — when the fade begins, so a
  picture that did not cover the screen crossfades in its margins too instead of
  snapping there. It is released the moment the fade arrives; a desktop never
  holds a screen-sized copy of a picture nobody can see.

Every rectangle of the layer paints through the same ground routine, so a cell
repainted for a hover mid-dissolve matches its neighbours exactly. An arrived
fade costs one blit, exactly as it did before there was a fade at all, and arms
no timer.

While it *is* dissolving, the whole layer is repainted each frame — the ground
genuinely changed everywhere, so this is what any whole-screen transition costs,
and it ends when the fade does. At login it overlaps the screen fade, which is
already recompositing the screen, so the marginal cost is the ground blit.

#### Only what changed is repainted

The desktop is the **bottom** layer, so marking all of it is never just a screen
repaint: every window above it recomposites, and every frosted backdrop over the
marked pixels is thrown away and blurred again. Repainting the whole layer to
move one highlight therefore costs, on a 1080p screen, most of a megapixel of
blur — felt as the pointer freezing for the best part of a second on a
Raspberry Pi 4B.

So the desktop's gestures report *where* they changed something instead of a
"redraw" flag. `Desktop::set_focused`, `pointer_moved`, `pointer_left`, `press`,
`context_press`, and `key` each take a `tairix_geometry::Region` damage sink and
add the **cell rectangle** of every icon whose appearance changed — the icon
that lost the hover and the one that took it, the old selection and the new, the
selected icon whose Focus Ring appeared or disappeared. An `IconTile` draws
strictly inside the cell the shared `GridView` gives it, so that rectangle is
the whole of repainting the icon; a gesture that changed nothing visible (a
focus flip with nothing selected, motion within one icon) adds nothing and
composes no frame at all.

`present_desktop_area` paints each of those rectangles under a narrowed surface
clip and hands the same rectangle to `Desktop::render`, which skips every cell
it does not reach, and `Compositor::repaint_desktop` marks exactly the
rectangles it painted. `present_desktop` is the whole-screen case of the same
call, for the changes that genuinely alter the whole layer: bring-up, a new
wallpaper, a theme switch, adopted settings, and a re-list that moved the icons
(which is why a re-list reports `relisted` rather than cells — no rectangle of
the old layout describes the new column). A freshly allocated layer is likewise
painted whole, since it holds no pixels a partial paint could preserve.

### The backdrop menu

A secondary press on the backdrop is the desktop's context-menu gesture:
`Desktop::context_press` selects the icon it landed on (so the menu acts on what
the user pointed at) or, on empty backdrop, leaves the selection exactly as it
was, and answers whether an icon was under it. It claims no keyboard focus,
because the window manager does not move focus for a secondary backdrop press
(`InputResponse::DesktopSecondaryPressed`) and the desktop does not pretend
otherwise. It names no `DesktopAction`: the menu is the seat's one chain, and
the embedder that owns the chain opens it.

**The desktop's own menu is a client of the [menu service](./menus.md) like any
application's**, and keeps no shell of its own. `pinboard::model` builds the
`ChainModel` the chain renders and `PinboardCommand::from_item` reads a chosen
row back; the plate, its title band, the placement, the grab, traversal and the
dismissal are all the chain's. The only difference from an application's menu is
where the model came from — built in process rather than decoded from the wire —
which is also what lets its rows say things an application structurally cannot
(that the *system* lacks the authority for a command).

Its command set is closed (`plans/PINBOARD.md` §7) — *Open* (only when the press
landed on an icon), *New Folder*, the four *Sort by* rows, the two *Arrange
from* rows, *Refresh*, *Open Desktop Folder*, and *Change Background…* — and
each row's id is its command's own position in `PinboardCommand::ALL`, so
leaving *Open* out shifts no other row's meaning and a reordering cannot re-map
what a row does. The sort order and the arrangement already in force are each a
group of alternatives, so the one that holds is drawn as that group's chosen
member (a bullet), non-actionable, with its reason stated: choosing what already
holds is a statement of where the desktop is rather than a command.

It opens through the same seat rule an application's `OpenMenu` resolves
through (`seat_menu_refusal`), so a backdrop press cannot take the grab from the
lock screen or the trusted picker by arriving from the other direction; a
refusal is stated on `stderr` and opens nothing.

The menu holds no authority: `Desktop::command` is the **one** place a command
becomes a `DesktopAction`, and *Open* resolves through the very same activation
the double-click and `Enter` paths use, so the three can never disagree about
what opening an icon means. A sort or arrangement row names
`DesktopAction::AdoptSettings` for the embedder to persist and hand back through
`apply_settings` (the model never adopts settings behind its back); *New Folder*
names `DesktopAction::CreateFolder` with the name already chosen through
`lib/browse`'s shared new-directory naming over the listing on screen;
*Refresh* re-lists there and then; *Open Desktop Folder* is an ordinary
`OpenFolder` activation; and *Change Background…* names
`DesktopAction::ChangeBackground`, which the embedder resolves to the installed
wallpaper chooser (the model knows no bundle paths).

The answer arrives at the chain's **one** delivery point, alongside every
application's, and is put through that same action path — so a chosen row and
the equivalent gesture on the icon column cannot diverge.

## Resolving taskbar responses

A `tairix_taskbar::TaskbarResponse` flows out of `DesktopShell::handle` as a
`ShellOutcome::Taskbar` value. The shell applies what its own state suffices
for — a `WindowChosen` outcome raises that window through the `TaskBridge`,
popup-internal changes just re-present — and the embedder (the `Run` binary)
performs what needs capabilities the shell does not hold
(`AGENTS.md` §10, §16.5):

- a press on the leading strip slot (the autostarted **file manager**) raises
  its served window idempotently: if a desktop-launched file manager is
  already running and serving a window, that window is raised and focused
  (`DesktopShell::raise_window`); if its launch is still in flight, the
  press is already satisfied; only otherwise is the bundle spawned.
- `AppDefault { app }` and `AppMenuChosen { app, item }` — relayed to the
  declaring process through the route its declaration recorded (see *The icon
  bar*).
- `AppRaise { app }` — the session raises that application's most recently
  used window; an application with none reported nothing in the first place.
- `ShowWindowPicker { app }` — the session builds one thumbnail cell per
  window and hands them back to the bar.
- `LibraryLaunch { entry }` — a chosen library entry, resolved through the
  catalog and spawned (see *The program library*).
- `CreateDesktopShortcut { entry }` — a link to that entry's bundle in the
  user's own `Desktop` folder (see *Desktop shortcuts*).
- `OpenLibrary` — the popup opened; the embedder re-reads the stores so the
  listing is current.
- `LibraryDismissed`, `NotificationActivated` — surfaced for the embedder;
  the bar's own state is already up to date.
- `SetDateTime` — the clock menu's set-time row was chosen; the session asks
  for an account that may set the clock (see *Asking for an account that
  may*), because it holds no such capability itself.

The bar's **context menu** is presented by the presenter as its own small
rounded window (a third window beside the bar and popup).

The capsule's system quick actions (`plans/NEW-TASKBAR.md` T13) arrive as
the same kind of typed outcome — the bar decides nothing and holds no
authority for any of them:

- `OpenSwitchboard { section }` — both the capsule's own press *and* the
  menu's two inspection rows (*About This System* → `System`, *System
  Monitor* → `Tasks`). Relayed to the live monitor; with none live the press
  is itself the demand for one, so an instance is brought up and the section
  held until its first publish proves it is listening.
- `LibraryLaunch { entry }` — the *Task Shell* row, which reuses the bar's
  one launch path rather than inventing a second. The row is actionable only
  while the terminal bundle resolves in the catalog the session handed the
  bar, so choosing it can never ask for a program that is not installed.
- `SetAppearance { appearance }` — the light/dark switch: re-theme the bar
  model, bring the desktop background in step, repaint, and redraw a prompt
  showing behind the menu, so nothing on screen is left in the appearance
  just left behind.
- `LockSession` — raise the [screen lock](#the-screen-lock). Any unanswered
  prompt is taken down first: a question must not sit behind a lock where
  the user cannot see what they would be agreeing to. A lock that could not
  be raised says so rather than leaving the user believing the screen is
  secured.
- `LogOut` — abandon any prompt and any lock, then unwind through the one
  owner-checked session release.
- `ConfirmSystemPower { action }` — never acted on directly; it opens the
  confirmation prompt below.

### Confirming a power transition

Restarting or powering the machine off ends every task of every principal on
it, so neither may follow from a single click. The requirement is carried in
the *type* of the outcome — the bar can only ask for a **confirmation**,
never for the transition — so the session cannot apply one without asking
first.

`confirm::ConfirmPrompt` puts the choice in a window the **session** owns,
drawn with the shared `lib/controls` dialog and modelled on the trusted file
picker: one slot (a second request while one is showing is refused rather
than stacking a second prompt), a session-owned compositor window at one
deterministic spot clear of the window-cascade slots, and a typed conclusion
the embedder acts on once the window is already closed. The safe button
leads the action band and holds keyboard focus when the prompt opens, so the
answer a stray `Enter` gives is "no"; the confirming button carries the
danger role. Every path that is not an explicit confirmation — the safe
button, `Escape`, or a request the session had to abandon for a lock or a
log-out — concludes as a cancellation, so nothing irreversible can follow
from a prompt the user did not answer. A prompt that cannot be shown asks
nothing and relays nothing, and says so.

Only a confirmed answer is relayed, and it is relayed rather than performed:
`switchboard::relay_power` sends one `SwitchboardCommand::Power` to the
monitor service's authenticated command mailbox, which performs the
capability-gated `system_power` syscall under its own identity. The desktop
session deliberately holds no power authority of its own — it is the
largest, most exposed process on the seat — and a relay that could not be
made states why on `stderr` instead of leaving the user thinking the machine
is going down. See [Switchboard monitor service](./switchboard.md#power-transitions).

## Switching the theme

`set_theme(ThemeId)` and `register_theme(Theme)` are the session's
programmatic theme controls; the interactive light/dark switch is the
`SetAppearance` outcome above, which resolves through the same path.
`set_theme` switches the registry and re-themes the taskbar in place; it
fails closed with `ThemeError::UnknownTheme` on an unregistered id, and
`register_theme` with `ThemeError::DuplicateId`, each leaving the active
theme and the taskbar untouched (`AGENTS.md` §5.4 / §2.9). An embedder that
switches the theme (through `DesktopShell::session_mut`) then calls
`DesktopShell::sync_background` and `present` to relay the switch to the
screen.

## Telling applications about their desktop

An application draws into its own frames, so it needs three facts the session
owns before it can draw them honestly: how large the screen is, what density
the desktop is at, and which way round the theme's colours run. All three
belong to the compositor — it owns the output it scans out to, that output's
`Scale`, and the active `Theme` — and `windows::desktop_info` reads them
straight from it into one `tairix_abi::desktop::DesktopInfo`. That single
definition serves both directions, so an answer and an announcement can never
disagree:

- `ShellWindowHost`'s `WindowHost::desktop` answers an application's
  `QueryDesktop`, which it issues before creating its first window. The query
  is read-only and carries no capability: the reply describes the caller's own
  seat, names no other principal's data, and grants no authority (`AGENTS.md`
  §5.2). A compositor whose output the record cannot describe — a zero-sized
  screen, a density outside the percentage the wire carries — refuses rather
  than answering with a guess (`AGENTS.md` §5.4).
- `announce_desktop` pushes a `WindowEvent::DesktopChanged` to every live
  window (`WindowServer::window_ids`) when any of it changes, routed through
  the same `deliver` path as any other event, so a client that has died is
  torn down here exactly as it would be otherwise. The interactive light/dark
  switch calls it: the session can re-theme its own surfaces, but an app's
  window is the app's pixels, so without the announcement the desktop would
  switch and every open window would stay in the appearance just left behind.

A desktop the record cannot describe is reported on `stderr` and nothing is
sent; each application keeps the last state it was given. See
[Variable DPI and UI scale](./dpi.md) and [Theming](./theming.md).

## Launch bookkeeping

The `launch` module owns the desktop's launched children. `LaunchTable`
remembers every child still running — its PID, the display label
diagnostics report it by, and the `Run` path it was spawned from (its
**attested bundle identity**: the desktop spawned the child itself, so no
window title or other app-controlled data is ever trusted for it,
`AGENTS.md` §23.1). `running_from` resolves the file manager's idempotent
open; `window_of_pid` (in the `Run` binary) finds the running app's served
window through the window engine's kernel-attested ownership records.
Asynchronous launch admits a child and returns its PID before the image
loads, so a load refusal surfaces as the child's reserved `LOAD_*` exit
status: the shared `reap_launched` drains every exited child in one wake,
reports each refusal on `stderr` named by its label (`launch_failure_report`
— fail loud, never fatal, `AGENTS.md` §2.24), tears down the child's
windows, and forgets the table entry.

### The launch argument vector

Every desktop launch is spelled through `launch_argv`, which names the
program's own path first and the caller's arguments after it. A program
reads its arguments from index 1 — index 0 is the program name its spawner
chose — so a launch that passed only its arguments would have its *leading*
one read as the program's name and never seen: the file manager's
`--desktop` role switch, the folder a desktop icon opens, the document an
icon launch names. One rule, in one place, for every launch site
(`AGENTS.md` §2.2).

## The Switchboard tray feed and hang detection

The taskbar's right-most capsule wears the signed-in account — the session
names it once at bring-up (`DesktopShell::set_account`) and the bar draws that
account's circular identity disc, the same one the login screen drew. The name
comes from `tairix_abi::ENV_SHOWN_NAME`, which login exports beside `USER`: it
is the account's *shown* name, so the desktop marks the person exactly as the
screen they logged in on did rather than deriving a mark from the login name.
The screen lock is headed by the same string for the same reason — it is the
login screen's own surface asking the same person. Behind the picture the
capsule is the Switchboard tray, rendering live state from two independent,
honest feeds (`plans/NEW-TASKBAR.md` T9/T10):

- **The published summary.** The `Run` binary spawns the Switchboard monitor
  service (`/System/Services/switchboard.app`, `SWITCHBOARD_RUN_PATH`) at
  bring-up as the logged-in user and binds the seat-scoped
  `SWITCHBOARD_ENDPOINT` beside the window and notification rendezvous. Each
  publish is attested against the caller's **own** launch record: the
  kernel-provided `call_peer_origin` pid must have an entry in the launch
  table, and that entry must name the service's bundle path — a foreign
  process, an orphan of an earlier session, or a hand-launched copy has no
  record of its own and is a typed refusal, stated on `stderr` and recorded
  in the system log, never rendered (`AGENTS.md` §5.4). The caller's *own*
  record is the authority, never the table's first entry for that bundle
  path: a session can hold more than one at once — an instance that exited
  but has not been reaped, or a replacement started over it — and a live
  instance must not be locked out by one of them. One monitor per session is
  instead enforced where it belongs, at the spawn: bring-up answers with the
  instance already recorded rather than starting a second.
  The decoded `TraySummary` reaches the capsule through
  `DesktopShell::set_tray_summary`; when the service exits, the reap path
  clears the feed (`set_tray_summary(None)`) so the capsule falls back to
  calm rather than freezing a dead service's last summary.
- **The session's own delivery evidence.** The desktop is the one component
  that observes whether an app drains its window events: every app-ward
  event is a non-blocking mailbox send. To avoid flooding an app with a dense
  gesture it must then drain one sample at a time from a bounded mailbox,
  `pump` folds an adjacent run of one gesture over one window — motion and
  interactive resize by latest-wins (a position and a geometry are both
  level-triggered, and a resize outcome is forwarded by reading the window's
  *current* extent after the whole batch has been applied, so every earlier
  sample of a run would carry the size the last one settled on) and wheel
  ticks by summing a run in one direction (a delta is additive, and a reversal
  ends the run) — while ensuring every sample still drives the window
  manager's own hover and drag state. A `ResizeEnded` is the settle the app
  must witness, so it ends a run rather than joining it. The production event
  sink folds each outcome into the `vigil::HangTracker` — an owner whose sends
  come back refused as the kernel's transient `WouldBlock` back-pressure
  signal continuously for `UNRESPONSIVE_AFTER_NS` (4 s) is flagged *not
  responding*, one accepted delivery clears it, and a reap forgets it (a dead
  app is not a hung app,
  and a recycled task id starts clean). No heartbeat is fabricated and no
  kernel query pretends to know. The loop drains the sink's change latch
  once per wake into `DesktopShell::set_tray_unresponsive`, which drives the
  capsule's danger state.

Both shell feeds re-present only when the capsule actually changed (the
bar's drained repaint latch), so an unchanged keepalive republish costs no
frame.

### Attesting the session to its monitor

A successful publish is answered with `encode_publish_reply`, carrying this
session's own `ProcId` — the identity the kernel attests to the process
itself (`tairix_rt::self_origin`), read once at bring-up and reused, never
re-derived a second way. It is the very reading the window server is
constructed with, so the identity a client learns from its `Create` reply
and the one the monitor learns here are the same one. That reply is how the
service learns the one identity whose commands it will accept, so the
reverse direction is authenticated too rather than trusted. A **refusal**
stays the plain status frame: an unattested caller learns nothing about the
session it failed to reach.

### The two owner-directed requests

Beyond publishing, the monitor's panel acts on *other* processes' windows,
and the session is the only component that may. Both requests are attested
exactly like a publish (the kernel-attested `call_peer_origin` pid against
that caller's own launch record) and then validated against what this
session can actually see (`AGENTS.md` §5.4):

| Request | Authorised against | Effect |
|---|---|---|
| `ActivateOwner { owner }` | the live window registry — the owner must hold a served window on this seat *now*, resolved through the window engine's attested ownership records | raises that owner's front window through the session's one focus/raise path |
| `RestartOwner { owner }` | the launch table — the owner must be a child this session itself spawned, so its bundle is the *attested* one it was launched from | re-launches that bundle through the session's one attested spawn-and-record path |

An owner this session cannot act on — an unknown or stale task id, a window
it does not serve, a process it did not launch — is `Errno::NotFound`,
stated on `stderr` and never guessed at. No refusal mutates the model, and
neither request adds a second raise or launch route: both re-enter the ones
the taskbar and launcher already use (`AGENTS.md` §2.2).

### The command mailbox the session sends on

The session sends to the instance's own mailbox,
`command_endpoint_for(<the service pid the launch table holds>)`, as a
**non-blocking** send — the desktop loop never blocks or spins on a panel
that is slow to drain (`AGENTS.md` §2.23). The send makes exactly one
attempt and answers whether the instance took it, and a refusal is reported
on `stderr` with the kernel's own reason: `WouldBlock` is back-pressure
from a mailbox the monitor has not drained, while `NotFound` is an instance
that has exited or has not bound its mailbox yet, and the two must not be
confused for one another. Three commands travel it, beside the `Power` relay
above:

- **`OpenPanel { section }`** — the capsule gesture. The bar decides the
  section (a quick press its running-task list, a press held past the bar's
  long-press threshold its recovery list) and emits it as
  `TaskbarResponse::OpenSwitchboard { section }` already spelled in the wire
  vocabulary (`switchboard_ipc::CommandSection`); the session relays that
  choice unchanged, so the section a user asked for is never re-decided a
  second time and there is no second section vocabulary to keep in step.
  A press is **never lost**, whatever state the
  service is in. With no instance live the press is itself the demand for
  one: the session revives the service through the same bring-up path and
  holds the section as *one* pending open — replaced, never queued — which
  is delivered on that instance's first publish (the proof it is up and
  listening) and cleared, so it is never re-sent on a later publish. An
  instance that is live but has not yet *bound* its mailbox refuses the
  send, and the press is held exactly the same way rather than dropped:
  this gap is real and wide, because the launch table names the service
  from the moment it is spawned while the process binds its mailbox only
  once its bundle has loaded — whole seconds on a cold boot, precisely when
  a user is most likely to reach for the capsule. Holding the gesture costs
  no retry loop: the instance's own publish, which it can only make once it
  is up, carries it through. A pending open the mailbox refuses through
  back-pressure is put back for the next publish rather than dropped.
- **`SeatReport { report }`** — the unresponsive-owner view of this seat,
  from the session's own delivery evidence above. It is sent **only when
  the tracked set actually changes** (the vigil's change latch, drained
  once per wake), never per frame and never polled. The report carries the
  truthful `total` count even when more owners are hung than a frame can
  name: the id list is bounded by `SEAT_REPORT_OWNERS_MAX`, so the monitor
  sees an honest count alongside the ids it can act on rather than a
  silently truncated one.
- **`FrameReport { report }`** — what the last composited frame cost, read
  straight from the compositor's own per-frame counters
  (`Compositor::frame_stats`). The session is the only party that can count
  it: the monitor samples the kernel, which knows nothing about pixels. It is
  sent on the same discipline as the report above — only with a live consumer
  and only when the counts changed — and a refused report is simply dropped
  rather than owed, because the next frame carries a fresher one. Nothing
  about it blocks or retries on a frame path. See [the Desktop
  block](./switchboard.md#the-desktop-block).
  - **Rate-limited, because this is the one report whose content churns at
    frame rate.** A change gate alone cannot quieten it: a pointer crossing
    bare wallpaper redamages the rectangle the cursor leaves and the one it
    arrives in, and those overlap by a different amount each frame, so the
    counts differ from the previous frame even though the desktop did nothing
    new. `FrameReportGate` therefore holds the send to at most one per
    `MIN_FRAME_REPORT_INTERVAL_NS` (250 ms) — the Resources page is read at
    human speed, and without the limit simply moving the pointer cost the
    monitor half a core rebuilding its model per frame.
  - **Still not polled, and nothing is lost.** A change the limit holds back
    tightens the session's own park to the moment it may go out
    (`FrameReportGate::park_deadline_ns`, folded through the same
    `park_within` as every animated surface), so a desktop that goes quiet
    mid-motion still reports its idle frame once and then arms nothing. A
    held-back report is always **re-read** rather than replayed: what goes out
    is the frame on screen at that moment, never the stale one it was
    holding.

## Publishing frame accounting to the System Information API

The report above is a **push to one reader** — the desktop's own monitor —
and reaches nothing else. Anything else that wants to know what the desktop
is repainting (a system monitor, `sysinfo frames`, a QEMU regression gate
asserting that a hover repaints a control rather than the screen) has no way
in, because the counters live in this process and the kernel every other
`sysinfo` query reads knows nothing about pixels.

So the session also publishes the compositor's cumulative
`DesktopFrameTotals` to `sysinfod`, which retains it against this process's
kernel-attested identity and serves it under `CAP_SYSINFO_GLOBAL`
(`docs/src/abi/sysinfo.md`, `plans/FIX-DESKTOP-SPEEDUP.md` A.4). The
submission is a process describing itself: it grants nothing, reads nothing,
and needs no capability.

`FrameStatsPublisher` is a separate gate from `FrameReportGate` because their
rules differ in kind. The monitor must not be told about the frame in which
it drew itself — measuring its own act of displaying would re-excite another
report for ever — whereas the retained accounting is a truthful count of
every frame the desktop composed, the monitor's own included. Nor does the
publisher read the monitor's liveness: a reader may arrive at any time, so
the figures are worth retaining whether or not one exists yet.

What the two gates share is the shape that keeps them off the frame path's
critical work: change detection by comparison, one attempt per
`MIN_FRAME_PUBLISH_INTERVAL_NS` (250 ms), a refusal dropped rather than
retried in place, and a held-back change tightening the session's own park
(`FrameStatsPublisher::park_deadline_ns`, through the same `park_within`) so
a desktop that goes quiet mid-gesture still publishes its final figures once
and then arms nothing. A desktop that has composed no frame publishes
nothing at all: the empty epoch is what the service reads as a withdrawal,
so sending it before there is an entry to withdraw would spend a hand-off
to say nothing.

**Neither gate ever waits for the service.** This runs at the end of the
compositor's own wake, and `ipc_call` parks the calling task off the run queue
until the far side replies, so a report here was a stall the user saw: the two
publishers together made up to eight cross-process round trips a second
through a gesture — measured at 5–39 ms each in the aarch64 hover vertical,
several whole frames apiece — and every application blocked in a window call
waited behind them. Both now hand the submission over
(`tairix_rt::submit::Submission`, `call_post`) and collect its verdict on a
later pass (`call_reap`), so the figure the service holds is still only ever
one it accepted, and the wake that collects it is the same one the rate
limiter already armed. A submission still outstanding when the next is due is
refused rather than queued, and its deadline retires it, so a wedged
`sysinfod` costs a restated figure and never a frame.

## Where a served window opens

An application never chooses its own position. `windows::placed_outer` is the
one placement rule: it takes the next slot of the diagonal cascade
(`windows::cascade_origin_for` — `CASCADE_ORIGIN` stepping by `CASCADE_STEP`,
wrapping after `CASCADE_WRAP` so late windows never walk off screen) and pulls
the whole decorated rectangle onto the **work area** — `work_area_excluding`,
the screen less the taskbar's band, the same rectangle a maximize fills, so
the two agree on what is reachable. `ShellWindowHost::window_opened` calls it,
and so does the QEMU verticals' host-side reconstruction of where a window
sits, so a scripted click and the guest can never disagree.

The cascade slot is a preference, not a placement. A window big enough to
overhang it from that slot would otherwise open with its right and bottom
edges off screen or behind the bar, where the pointer cannot reach them — the
invisible resize edges among them, which is what made resizing look broken on
every window after the first on a small display. The clamp runs once the
decoration band is on and the outer rectangle is therefore known, so it
measures the real window rather than re-deriving the frame's insets
(`AGENTS.md` §2.2); a slot that already fits moves nothing and marks no
damage. A window larger than the work area in an axis is pinned to its start,
so the title bar and the leading edge stay reachable whatever else is not.

## The app-ward hold-back

A refused send is **owed**, not lost. Back-pressure says the app is behind,
not gone, so the session keeps the event and delivers it when the app
catches up (`holdback::HoldBack`). Dropping it would cost the app something
it cannot re-derive: a `Resized` it never sees leaves it laying out and
hit-testing at a size the compositor no longer uses, and a lost
`FilePicked`/`PickCancelled` strands that window's one pending pick for the
rest of its life, because the engine clears the pick only once the
conclusion is *accepted*.

- **Parked, never polled.** The first debt to a destination arms a
  `WaitSourceKind::PortRoom` member on the app's own event port — the
  send-side twin of the `Port` member, admitted by the send authority
  `ipc_send` itself checks. The app's own drain frees a slot and the kernel
  wakes the session, which sends what it owes; the member is dropped the
  moment the destination owes nothing. The desktop never spins on capacity
  and never blocks on an app (`AGENTS.md` §2.23, §4).
- **Order is the app's, not the mailbox's.** A destination already owed
  something takes the next event unsent, so nothing overtakes what is
  queued. Queues are per `(destination, window)` and a flush serves an
  owner's windows round-robin, so one window's backlog cannot starve a
  sibling's resize.
- **Folding by what the quantity means.** A state edge (`Focus`, `Resized`,
  `RedrawRequested`, `CloseRequested`, `Minimized`, `DesktopChanged`) is a
  value the app converges on, so a later one replaces the held one where it
  stands and a window owes at most one of each. A position is
  level-triggered (newest wins) and a wheel run is additive (a reversal ends
  it) — the same rule `pump` applies live, from the one shared predicate.
  Everything else — keys, buttons, the pick conclusion — is owed in full.
- **Bounded, and shedding only what can be shed.** `HOLD_BACK_CAPACITY` (64
  per window) is what stops an app that never drains from making the desktop
  hold memory on its behalf: a security bound, not a capacity to scale
  (`AGENTS.md` §24.4). Overflow evicts the oldest *input* event, which is
  total rather than a preference — folding leaves at most six state edges
  and one pick conclusion, so an evictable input event always exists.
  Oldest-first is also the safe direction for a button: a press is shed
  before its release, so an app can be left with an unmatched release,
  never an unmatched press it would hold as a latched grab.
- **A dead owner owes nothing.** A send that answers `NotFound` discards
  everything held for that owner and tears its windows down, exactly as a
  refused direct send does; a reap does the same.

## Presenting the taskbar through the window manager

The taskbar paints a *rectangular* `tairix_raster::Surface` and the window
manager composites and rounds windows; neither depends on the other
(`AGENTS.md` §17.4). `TaskbarPresenter` is the session's glue between them.
Given a `&mut tairix_wm::Compositor` and the taskbar's own
`tairix_taskbar::TaskbarRenderer` (which holds the across-frame glyph cache),
`present` paints the bar and, while the program-library popup is open, its
panel, and presents each as a compositor window:

- the bar is placed at `BarLayout::bar`'s origin and rounded with
  `Corners::from_radius(BarLayout::corner_radius)` — the compositor's single
  anti-aliased rounded-corner path, the same one it uses for application
  windows, never a second one (`AGENTS.md` §2.2);
- while the popup is open its panel is placed above the bar at
  `LibraryLayout::panel`'s origin and rounded with its `corner_radius`;
  closing the popup removes the popup window;
- the hover window picker, the notification popover, and the Switchboard
  capsule's instrument readout are presented the same way while each is
  open (`PickerLayout` / `NotificationsLayout` / `TrayReadoutLayout`), and
  each window is removed the moment its surface closes.

Every one of them is placed asking for the theme's `chrome_backdrop_blur`,
because each is drawn on the session's floating ground — as is every surface of
an open menu chain, which `DesktopShell::present_menu_chain` reconciles rather
than the presenter ([menus](menus.md)).

`present` repaints **only what the taskbar owes**. It takes the
`tairix_taskbar::TaskbarRepaint` that `DesktopShell::present` drains from the
model, which carries one `Repaint` account per surface, and brings each surface
up to date with what its account names. Two things override an empty account —
a surface that has no window yet is always painted, so the first frame puts
everything on screen, and a change of desktop density repaints everything,
because every rectangle owed was measured at the old density and the scale
belongs to the output rather than to the taskbar model.

**A surface already on screen keeps its pixels and is repainted only where it
owes them.** The presenter hands the compositor a *paint* over the window's
retained buffer (`Compositor::repaint_window`) rather than a freshly rendered
surface, so the compositor marks the rectangles that were painted instead of
the window's whole bounds — the same treatment `present_menu_chain` gives a
menu plate. That is the difference between a 40 × 40 slot taking the pointer's
wash costing its own 1600 pixels and costing the bar's 40 560, over a frosted
backdrop that must then be re-blurred. The renderer re-derives its whole recipe
under each rectangle as a clip (`tairix_controls::damage::paint_parts`), so a
scoped repaint lands exactly the pixels a whole paint would have laid there and
there is no second "paint just this control" recipe to disagree with the first;
a window whose extent no longer matches the layout is repainted whole into a
buffer of the new size, since a buffer of the wrong size has nothing for a
partial paint to keep.

The presenter owns only the compositor `WindowId` tokens it minted — the
taskbar model, the renderer, and the compositor are the embedder's, so the
session composes the GUI crates without owning the window-manager handle. It is
total and fails closed (`AGENTS.md` §2.9): a paint whose pixels cannot be
allocated leaves the on-screen window untouched rather than blanking the bar
*and keeps what that surface owed*, so a refusal can never report a stale
surface as current; a window the compositor no longer knows is re-created on
the next present; and `teardown` removes every window so a session shutdown
leaves nothing orphaned.

## Fading the desktop in and out

The login screen fades to black and exits, so the screen a session inherits
is already dark. `ScreenFade` fades the desktop up out of that black rather
than snapping it on: it starts the compositor's [screen
reveal](wm.md#screen-reveal) at `0` and walks it to `u8::MAX`. Logging out
and stepping aside for another account run the same fade the other way, so
the desktop dissolves into the black the login screen appears out of — the
seat is handed on cleared, and the two ends of the switch meet on the same
colour rather than one of them cutting.

The span is the active theme's own `SessionFade` duration, read off the
compositor's theme, so no timing is spelled in the session. Motion state is
the shared `tairix_theme::Fade` and nothing else, which is what makes the
degenerate cases free of any branch here:

- **Reduced motion** answers a zero duration, which starts settled. The
  desktop is simply fully revealed — or simply black — on its first frame:
  no extra present, no timer.
- **A clock that jumped backwards** settles rather than stalling.
- **A fade turned around** picks up the strength that is actually on
  screen, so a log-out chosen while the desktop is still revealing dims
  from where it had got to rather than flashing bright first.

The reveal begins at the session's first successful present, not at
bring-up: the span is wall-clock, and a fade started earlier would spend
itself on a screen with nothing on it yet.

It is driven by the run loop's existing park, never by a frame timer. Each
wake steps the fade to the current instant, and the park the loop was
already going to use is shortened to its next frame — `park_within` is the
one fold both this and the lock screen use, so the two cannot round a
deadline differently. **With nothing animating the park is byte-for-byte
the value the loop already carried**, so an idle desktop arms no timer at
all.

Time alone drives it: reaching the end of the span puts the screen at that
end and settles, whatever became of any frame presented on the way. A
display that refuses a present mid-fade therefore cannot strand the desktop
part-lit, and nothing retries.

### Departing, and coming back

The departure cannot ride the serve loop, because it has to *finish* before
the seat is given up. Nor may it park on the session wait-set: the sources
it is no longer serving would report ready on every re-park and spin a core
through the whole fade. `fade_to_black` drives it instead — present a
frame, sleep to the next one on `tairix_rt::park_ns` (the runtime's
off-CPU timed park), step, repeat — bounded by the fade's own span. A
refused present stops the dim where it got to; the seat is handed on
cleared regardless, so the screen still ends black.

`SeatPresentation` fixes where the two fades sit in a user switch. Going
out: `fade_out`, then `suspend`, then `release_seat` — the dim runs only
after the authority *accepted* the step-aside (a refusal must leave the
screen exactly as the user left it) and while the seat and frame ring are
still up, so nothing is cut off mid-frame. Coming back: `acquire_seat`,
`query_mode`, `reconfigure`, `fade_in`, `repaint_all` — the arrival is begun
before the repaint, so the frame it presents is the first of the fade and a
resumed session appears out of black exactly as a fresh one does instead of
returning to a black screen or snapping on.

### The one-shot visible witness

Reaching full strength is announced once per session as `DESKTOP_REVEALED`
("desktop fully revealed on screen"), emitted after a present that reached
the display. It marks the desktop *visible*, which "a frame was presented"
no longer does: every frame before it is black to a degree, so an observer
keyed on the first present cannot tell a revealing desktop from a blank
screen. Under reduced motion the first frame is already at full strength
and the witness lands there, so a consumer waiting on it never waits for a
fade that will not happen. It is said once and never repeated, and a fade
heading for black never says it at all: it reaches black, not visibility.

It also waits for the desktop's **first backdrop**. The wallpaper is read and
decoded on a worker thread, so the frames before it lands carry the fallback
colour where the user's picture belongs — which is no more "the desktop" than a
half-faded frame is. `ScreenFade::set_awaiting_backdrop` holds the witness until
that first preparation resolves (installed, refused, or never wanted) **and**
until the picture it installed has finished dissolving in, and the frame that
carries that is the one the witness follows.

Its id and message are defined once beside the reveal and imported by the
desktop QEMU verticals, which gate their screendump on the rendered text,
so emitter and consumer cannot drift.

Emitting it needs `CAP_LOG_EMIT`, which the bundle's manifest requests and
the interactive-account ceiling carries, so the intersection keeps it. The
same grant is what finally lets the session's cache ledgers reach the log:
they were wired to it long before any account could hold it, and were
silently discarded until now.

### Each icon-bar action the session relays

`APP_BAR_RELAYED` ("icon-bar action relayed to its application") is emitted
once per relay, naming the target's opaque `ProcId` and whether the action was
the slot's `default` or a `menu` row. It exists because the count of relays a
*single* gesture produces is a fact the bar's own state cannot answer: the
router reports one action per press, so whether a press arrived twice shows up
only here. It names no pointer position and no key, so it carries no input
content.

### Each served window's own visible witness

`DESKTOP_REVEALED` says the desktop is visible; `WINDOW_SHOWN` ("served
window first frame on screen") says one *application's* window is, naming it
in a `window` field. It is emitted from the same place and
for the same reason: after a present that reached the display, because until
that frame lands nobody has seen the window.

The session is the only component that can say this. An application learns
that its present was *accepted* and the compositor that a frame was
*composed*, but neither sees a composed frame reach the display. Nor can an
observer of the window channel work it out: a present, a backdrop-blur
change, a retitle and an icon-bar declaration all answer with the same
four-byte status reply, so "the reply after the create is the first present"
is a guess about how many requests an application happens to make — and, on
a shared rendezvous, about the other clients too.

A window is announced only once its own first present has landed. Before
that its body is the session's opening fill rather than the application's
pixels, so a frame carrying it shows a blank window: `SessionWindows` tracks
each window as awaited, then painted, then shown, and only the painted → shown
step announces. A refused present leaves it awaited.

Once, and once again after a release. Releasing a hidden window's content makes
the record's claim false — the window composites as an empty plate, so nothing
of the application's is on screen — so `SessionWindows::content_released` puts it back
to awaited and the frame that brings its re-attached pixels back announces it
afresh. Without that, a window released and never re-presented (an application
that ignores its redraw) would still read as shown.

Two readers depend on it. A user diagnosing an application that launched but
showed nothing can tell "never drawn" from "never launched". And the icon-bar
QEMU vertical gates both its screendumps and its bar gestures on it: a create
reply would say only that the window exists, which is too early to photograph
and too early to click.

### And the trusted picker's own witness

`PICKER_SHOWN` ("file picker on screen") is the same announcement for the one
surface the session puts up on an *application's* behalf. It is needed for the
same reason `MENU_SHOWN` is, and for one more of its own.

The picker is a window the session owns, so the window channel says nothing
about its pixels, and the application that asked learns only that its
`PickFile` was *accepted*. But acceptance is not readiness here: the picker
lists its directory through the session's deferred listing desk, so it can be on
screen showing its "listing…" cue with no row to choose yet. So the
announcement waits for both — a frame carried the picker *and* its listing has
landed — and until then says nothing.

One announcement per pick, not per session: the next pick is a picker the user
has not seen, so it is announced in its own right. A waiter that keyed on the
first announcement alone would otherwise act on a stale one.

Two readers depend on it, as with its siblings. A user diagnosing a pick that
never appeared can tell "never drawn" from "never asked". And the
picker-delegation QEMU vertical gates its pick-click on it: any earlier gate
races the frame the rows are in.

### And its own release witness

`CONTENT_RELEASED` ("window content released under memory pressure") is the
other end of the same story: the session emits it, naming the `window` and the
`bytes` given back, when it unmaps a hidden window's frame region and tells the
owning application it may let go of its own copies
([memory](../architecture/memory.md#window-content-is-a-policy-not-a-cache)).

Every other reclaim decision on the machine is already recorded — a cache's
evictions and refusals through `lib/reclaim`'s audit sink, the band itself by
the kernel — and window content is by far the largest block the desktop gives
back, so a silent release would leave the one that matters most the only one
nobody can see. It is also the only reclaim a *user* can perceive, because the
application is asked to re-establish its pixels afterwards.

## The screen lock

Choosing *Lock Screen* in the taskbar's [system quick-actions
menu](taskbar.md#the-system-quick-actions-menu) secures the display behind
the signed-in user's password (`lock::ScreenLock`,
`plans/NEW-TASKBAR.md` T13). Locking is the one way out of a session that
*keeps* the session: every application carries on running — a build
finishes, an editor keeps its unsaved buffer — but nothing on screen is
legible and no input reaches any of it.

A modal window would not be a lock, because a window can be moved, lowered,
or clicked past. Three properties carry it instead, and all three are
load-bearing:

- **It covers the screen.** The surface is the compositor's full extent and
  fully opaque, so a passer-by learns nothing about what is on the machine.
- **It takes every event.** While `ScreenLock::is_locked`, the session drains
  the seat's pointer and keyboard *straight into the lock* rather than
  through `DesktopShell::pump`/`handle`, so no motion, click, or keystroke
  reaches the window manager, the taskbar, a served application, or the
  confirmation prompt. When a password is verified part-way through a drained
  batch, the remainder of that batch is drained and **discarded**: it is the
  tail of the gesture that typed the password, and delivering it into the
  session the instant it becomes visible would leak part of a password entry
  into whatever holds focus.
- **It stays on top.** `keep_topmost` raises it immediately before every
  composite, so an application that opens or raises a window behind the lock
  cannot surface over it.

The lock holds no authority to authenticate anybody. It offers the typed
password to the per-console **elevation broker** — the login supervisor that
started this session — through the one shared client, `tairix_rt::elevate`,
as an `ElevateRequest::Verify`. The broker attests the caller's identity from
the kernel, checks the password against *that* uid, audits the decision, and
answers; the lock believes only `Verified`. A refusal, a transport failure, a
broker that is not there, and a reply it cannot parse are all one answer:
still locked (`AGENTS.md` §5.4). It deliberately keeps no attempt counter and
no rate limit — the broker owns that policy and audits every attempt, and a
second copy here would be a second place to get it wrong (`AGENTS.md` §2.2).

The password lives in exactly one place, the masked field's own bounded,
pre-reserved buffer, and is erased on every path out of the prompt: verified,
refused, unreachable, or abandoned when the session tears down. The field is
`tairix_controls::TextField` in secret mode — the one shared text control,
never a second text entry — which draws one bead per character rather than
the characters themselves, reserves its buffer once so typing can never
reallocate and strand a copy of the secret in a freed block, and redacts
itself in `Debug`. The erase is the workspace's single volatile
`tairix_util::secret::wipe`, which an optimiser cannot delete as a store
nobody reads back.

The lock screen is the login screen's own surface, so it animates like it:
the session hands it the real monotonic clock on every event and steps it
with `ScreenLock::advance` on every wake, and `ScreenLock::park_deadline_ns`
folds whatever the surface asks for into the loop's park through the same
`park_within` the desktop's reveal uses. A refused unlock therefore shakes
the question as it does at login. An idle lock screen asks for nothing and
arms no timer.

Whether the row is offered at all is the session's attestation: it tells the
bar through `DesktopShell::set_elevation_available` whether it runs on a
console that has an elevation endpoint to unlock with. The bar refuses the
row until told otherwise, because a lock that could never be undone would
strand the user rather than protect them. The same one attestation governs
the clock menu's set-time row below, which needs the same broker.

## Asking for an account that may

Some commands a desktop offers are ones the session itself may not perform.
Setting the machine's date and time needs `CAP_TIME_SET`, which is not in a
session's manifest and must never be. So the session does not perform it: it
asks for an account that may, and the per-console elevation broker does the
rest (`elevate::ElevatePrompt`, `plans/NEW-TASKBAR.md` T17).

Choosing *Set Date & Time…* in the [clock's
menu](taskbar.md#the-clocks-menu) opens the session's own credential prompt —
a session-owned compositor window, drawn with the shared dialog and two
shared text fields, so a password is typed into desktop chrome and never into
an application. One prompt shows at a time; a second request while one is up
is refused rather than stacking a question over the one already asked. While
it is showing, the prompt consumes its own window's keys and clicks, so no
keystroke of a password reaches whatever held focus behind it.

What the prompt does with an offer is post it, and nothing more. It sends
`ElevateRequest::Launch` through the one shared client, `tairix_rt::elevate`;
the broker re-authenticates the named account exactly as a fresh login would,
audits the attempt, and starts `datetime.app` **as that account**. The
session learns the started pid or the refusal, and never the reason behind a
refusal — the broker answers a wrong password, an unknown account, and a
locked account indistinguishably. The blocking `Run` exchange would not do
here: its reply arrives only once the elevated program has exited, so a
desktop posting it would stop serving windows to the very program it is
waiting for. The started program is *login's* child, so login reaps it; the
desktop could not wait on it in any case.

Every path out of the prompt fails closed or states itself:

- An **empty field is never offered**. There is nothing to check, and asking
  would spend an audited attempt against the account; the keyboard moves to
  whichever field is still empty.
- A **refusal keeps the prompt up** with the reason stated and the password
  cleared — the masked field zeroises what it discards — so a retry starts
  from empty with the account name kept, which is not a secret.
- A **refused authentication and a program that would not start read
  differently**, so a user whose account *was* accepted is never sent back to
  re-check a password that worked.
- A **cancellation says so** on `stderr`: a user who asked to set the clock
  and saw nothing happen is told that nothing was set.
- The prompt is **abandoned unanswered** when the screen locks, the session
  steps aside for another user, or it ends — and the secret goes with it,
  since the field zeroises its buffer as it is dropped.

The password is held in exactly the same one place, and by the same shared
masked field, as the screen lock's (above): there is no second credential
buffer and no second text entry anywhere in the session.

## Routing one seat's input to the taskbar and the window manager

The desktop has two input routers — the window manager's
`tairix_wm::InputRouter` (focus, click-to-activate, interactive move- and
resize-grabs, every application window and the desktop layer behind them) and
the taskbar's `tairix_taskbar::TaskbarInput` (the bar, its program-library
popup, the hover window picker, the notification popover,
and the Switchboard capsule's readout) — and both consume the **same** shared
`tairix_input` event vocabulary (`AGENTS.md` §17.4, §2.2). A real input source
produces one stream, so `SessionInputRouter` is the glue that fans it to the
right router: the session's **input seat**, routing the pointer and keyboard of
the seat the session owns ([seat ownership](seat.md)).

### Why a seat, and not two routers guessing

Each router knows its own geometry. Neither can see the *stack*, and without
the stack geometry is not an answer: the bar's clock stays at the bar's
coordinates when a window is dragged across it, so a router that hit-tested
only its own rectangles would act on gestures the user aimed at the window in
front of it — a click doing something on a control that is not even visible,
hover feedback lighting up beneath someone else's window, a popover opening
over it. Nothing pins the bar topmost; it is an ordinary compositor window, and
every application window is raised above it the moment it is opened or clicked
(`Compositor::raise`).

So the seat owns the two facts neither router can: **where the pointer is**,
and **which surface it rests on**. It resolves the second before it delivers
anything, hands the event to that one router, and tells the other that the
pointer has left — `tairix_input::PointerFocus`, the enter/leave pair every
window system needs, for exactly the reason every window system needs it.

### The policy

`handle(event, &mut Compositor, &mut Taskbar, &TaskbarPresenter, now_ns)`.
The presenter is read, never driven: it is the only thing that knows which
compositor window each taskbar surface *is* (`TaskbarPresenter::owns_window`),
which is what tells "the bar is under the pointer" from "a window covering the
bar is". The monotonic `now_ns` is threaded down because one taskbar gesture is
decided by *time*: the Switchboard capsule distinguishes a tap from a hold by
how long its press has been down when the next event arrives, so the bar is
given the embedder's clock reading rather than reading a clock of its own.

Every pointer event takes the same three steps, in this order, and there is no
fourth — **resolve** who holds the pointer, **move the focus** there, then
**deliver** to that one router:

1. **A modal surface of the bar's holds the pointer.** While the bar's context
   menu or its program-library popup is open, every pointer event and every key
   routes to the taskbar, wherever the pointer is. That is an *active grab* in
   the ordinary window-system sense, and it is what lets a press anywhere
   dismiss the surface (the click-away) without also acting on what it landed
   on. Nothing leaks to the windows beneath.
2. **A held button holds the pointer.** The first press takes an implicit grab
   for whichever surface it landed on, and every event up to the release of the
   *last* button goes there — so a window drag that runs under the bar keeps
   dragging, a capsule press that slides off the capsule still resolves on the
   capsule's own terms, and a release can never be claimed by a surface that
   did not see the press. There is no "offer it to one and then the other".
3. **Otherwise the stack decides.** The surface `Compositor::window_at` finds
   drawn under the pointer gets the event: the taskbar when that window is one
   the presenter placed, the window manager for anything else — an application
   window, a session dialog, the lock screen, or the desktop layer when there
   is no window at all. The test is per event position, never per surface: a
   window covering the bar's trailing end leaves every button it does not cover
   still the bar's.

Two consequences worth stating on their own:

- **Motion is delivered, not fanned.** Only the surface holding the pointer is
  told the pointer moved, so only it updates hover feedback and resolves
  pointer gestures. The other is told the pointer *left*, which is the only way
  a hover can end: when a window rises over a hovered control the pointer has
  not moved at all, and re-testing its unchanged position would answer "still
  hovered" and strand the highlight — and any popover it opened — over the
  window now in front of it. The taskbar's `set_pointer_focus` drops every
  hover and starts the window picker's closing grace (a window passing over the
  bar and moving on cancels it again, so the panel does not go down behind it);
  the window manager's puts out the title bar command the pointer was on.
- **Keys follow the keyboard, not the pointer.** They go to the window manager,
  which delivers them to the focused window; the taskbar takes them only while
  one of its modal surfaces is open. A pointer resting on the bar never diverts
  a keystroke from the window the user is typing in.

A press or motion that the holding surface did not act on is
`SessionInputResponse::Ignored`.

### Re-resolving when the stack changes

Which surface the pointer rests on depends on the window stack, so it goes
stale whenever the stack does — a window opened, closed, raised, moved or
hidden, a popover of the bar's placed or removed — and none of those is a
pointer event. `refresh_pointer_focus` re-resolves and moves the focus if the
answer changed, and `DesktopShell::present` calls it: every change to what is
on screen ends in a present, so that is the one place the answer can be
refreshed without a caller having to remember to. It runs *before* the paint,
because the stack it resolves against is the one currently on screen, and
because a hover it drops or takes up latches a repaint that this very present
then draws. An in-flight grab pins the answer, so it can never take the pointer
away from a drag mid-gesture.

An *arrival* deliberately does less than a motion: it refreshes the hover under
the pointer but opens no hover surface. A window closing is not a gesture, and
a popover that appeared because something else vanished is one nobody asked
for; the next real motion opens it.

### Giving the pointer up

The seat's implicit grab is a function of the presses and releases it *sees*,
and the desktop has two modal surfaces the seat does **not** route: the screen
lock and the pinboard's backdrop menu. Both drain the seat's channels straight
into themselves, so that nothing behind the plate can be reached — which means a
gesture in flight when one opens ends with a release the seat is never given.

`DesktopShell::yield_pointer` is how the embedder's drain says so: the gesture
ends and both routers are told the pointer has left. Without it the seat would
hold a grab for a button that can never come up, and the pointer could never be
resolved against the stack again — the bar would be unreachable for the rest of
the session. Dropping the focus at the same time is what stops the bar sitting
there with a lit control behind a lock screen. It is idempotent, so each drain
says it on every pass rather than working out which pass was the first, and the
stream coming back needs no announcement: the next event resolves the pointer
afresh and the next press starts a new gesture.

### What this is worth beyond correctness

The bar is *trusted* desktop chrome: its menus can offer to lock the screen, to
log out, to re-authenticate for a privileged application. An unprivileged
window that could drive that chrome's state — provoke a popover to appear over
itself at a moment it chose, or have a click it received acted on by a control
the user could not see — would have a user-interface redressing primitive.
Resolving every pointer event against the stack, in one place, is what denies
it: chrome reacts only to input the user actually directed at chrome. It is the
same reason the lock screen is safe here — its window is not one the presenter
placed, so while it is up the bar cannot be reached by the pointer at all.

## What an application's presented frame costs

An application repaints its whole composition and presents whole-window
damage, because a toolkit generally cannot say which pixels its own paint
touched. Taking that claim at face value would recomposite every pixel of the
window for a hover highlight a few rows tall, so the session measures the
truth instead: `ShellWindowHost::window_presented` converts the presented
pixels into the compositor's own content surface and returns the bounding
rectangle of the pixels whose value actually differs, and only that rectangle
is marked dirty. A repaint that changes nothing marks nothing.

The comparison is exact — a pixel reported unchanged carries the
byte-identical value it already had — and it rides a loop that already reads
the frame and writes the surface, so it adds one read per pixel and no
allocation. It fails closed: every index the conversion will use is validated
before the first write, so a malformed or hostile geometry refuses the whole
present rather than leaving the window half-converted.

Both directions of that conversion are one definition,
`tairix_display::winframe`, beside the channel-order decision the scan-out path
already owns: an app writes its surface out through `encode` and the session
reads it back in through `decode`. And because it is the one whole-window pass
the session *cannot* bound — the app declares the damage — its rows are spread
across the same participants a composite uses, read back from the compositor
(`Compositor::job_runner`) so the two share one answer about how wide the
machine is.

## One composite per frame deadline

The loop wakes as fast as its sources produce work, and a hand on a mouse
produces pointer samples several times faster than any screen shows a frame.
Compositing once per *wake* therefore spends whole frames' worth of blending on
pixels the next sample overwrites before a scan-out ever reads them.
`FramePacer` (`src/pace.rs`) is the frame deadline that stops it: damage
accumulates in the compositor between deadlines, and `FramePacer::admit` lets
the loop composite and present once a deadline arrives.

- **Latency is paid only where a frame would have been wasted.** A frame whose
  period has already elapsed — the first after an idle desktop, a click, a
  keystroke, any interaction slower than the screen — is admitted on the very
  wake that produced it. Only a producer outrunning the display is held, and
  only until the frame it is racing.
- **One shot, never a tick.** A held frame shortens the loop's park to the
  moment it comes due, through the same `park_within` fold the clock, the
  reveal, the lock and the frame report use. Nothing held arms nothing: an
  idle desktop parks on exactly the deadline it would have had, and the pacer
  costs it not one wake. `admit` holds only a frame that is *not* yet due, so
  the deadline it arms is never zero-length and the loop cannot spin between a
  refusal and its frame.
- **The period is the one the desktop already animates at.**
  `tairix_theme::Timeline::FRAME_NS` is the shortest gap between two frames
  worth drawing, which is the same fact whether a frame carries an animation
  step or a drag. Sharing it also keeps an animated surface from being woken
  for a frame the pacer would then refuse.
- **An undamaged frame is never held, and never starts the period.**
  Presenting one moves nothing, and it is what re-reads the frame counters as
  idle for the monitor's Resources page — so holding it would suppress that
  reading, and starting the period would put the next real frame behind a
  frame that changed no pixels.
- **The compositor owns the damage; the pacer owns only the clock.**
  `Compositor::has_damage` answers whether a composite would recompose a
  pixel, so the two cannot disagree about whether a frame is owed. A clock
  that jumped behind the last frame admits rather than freezing the screen for
  the length of the jump.

The fade out taken on the way to a log-out or a user switch is *not* paced: it
runs on its own timed park with the seat still held, because it is the last
thing the session draws and must complete before the screen is handed on
(see [fading the desktop in and out](#fading-the-desktop-in-and-out)).

Real vsync — a deadline taken from the flip a display driver signals rather
than from a fixed period — is where `plans/FIX-DISPLAY-ACCELERATION.md` takes
this next. No display driver reports a refresh today, so a mode field for one
would be an ABI with no producer.

## Nothing the desktop reads or writes happens on the serve loop

The serve loop owes the user a frame, so it performs no blocking I/O at all: not
in response to input, not while painting, and never because a control's value
changed. Five things it needs are I/O of arbitrary length on arbitrary
hardware — listing a directory (`fs_open` + `fs_readdir`), preparing the
wallpaper (a bounded read, then a sandbox round trip), decoding an icon,
publishing the user's desktop settings (a store round trip through the app-data
service), and reading the program catalogue (two documents plus one `AppInfo`
per catalogued application). Run on the session's own task any one of them
stalls the compositor, the seat drain, and every application blocked in a window
call for as long as the disk takes. All five therefore run on `lib/rt` worker
threads.

The arrangement is the same shape five times, and the shape is a **desk**: a
host-tested state machine (`tairix_browse::ListingDesk`, `WallpaperDesk`,
`tairix_icon::ArtworkDesk`, and `tairix_util::defer::JobDesk` for the settings
publish and the catalogue scan) holding what each consumer asked for, what has
come back, and the staleness rule that discards an answer for somewhere the
desktop has since left. The `Run` binary adds the three
things a real program brings — the runtime's futex mutex for exclusion, a
condition variable the worker parks on with nothing to do, and one byte on a
pipe whose read end is a wait-set member. So the session learns an answer landed
through the very loop it already parks in: no new ABI, no second wake mechanism,
and nothing anywhere spins.

**A desk hands a job out by taking it, never by copying it.** The request and
the answer are one thing: storing an answer clears the request it answers, so a
worker that has just delivered finds no work and parks. Leaving the request
standing is what turns a desk into a busy-poll — the slot becomes workable
again the instant it is answered and the serve loop runs the same job again for
ever. Two desks did exactly that, and the desktop's listing worker read one
folder about 150 times a second, waking the compositor on every completion
(`plans/FIX-DESKTOP.md` DESK-17).

- **Two listing consumers, named rather than counted.** The icon column and the
  trusted file picker each have their own slot, and the worker serves them
  round-robin, so a picker walking a deep tree can never hold the icon column's
  re-list behind it.
- **The picker gains a real pending state.** `Browser` records the navigation
  and *moves nothing* — not the location, not the entries, not either history —
  until the listing arrives; `Browser::resume` commits it. A listing that is
  refused when it does arrive drops the pending navigation and leaves the view
  exactly where it was, so a refusal is reported in place rather than stranding
  the user in a directory the session could not read. While a read of somewhere
  *else* is in flight the listing area says so (`Listing…`), because the items on
  screen belong to a directory the user has already asked to leave; a re-read of
  what is already shown keeps its items, so a periodic re-list cannot flicker.
- **The wallpaper's worker owns its own sandbox.** The icon rasteriser keeps the
  loop's own sandbox handle, untouched and deliberately not `Send`; the wallpaper
  thread creates a second capability-empty worker inside itself, so no sandbox
  handle ever crosses a thread boundary.
- **A refusal travels with the answer.** `stderr` is one descriptor and a
  formatted line reaches it in several writes, so a worker stating a reason where
  it noticed it could interleave with anything else writing at the same moment.
  The worker answers *why* instead; the serve loop states it, once, on its own
  thread.
- **A refused thread is not a failure.** A kernel that grants no thread, or a
  pipe it refuses, is stated once and that work happens on the serve loop —
  exactly where it used to be. Slower under load, never wrong.
- **A settings change is published off the loop, and adopted only once it
  landed.** Both routes into the desktop's settings — a row chosen from the
  backdrop menu, and an `Apply` from the wallpaper chooser — submit to the
  settings worker and adopt nothing. The worker publishes to the desktop's own
  app-data scope and answers with what the store then holds; the serve loop
  adopts *that* on the wake it nudges, and re-lays-out, re-lists, and re-prepares
  the wallpaper only for the change the answer actually names. So the adopted
  state and the published document can never diverge, and neither can freeze the
  desktop. A refused publish is stated on `stderr` and adopts nothing, leaving
  the desktop showing the settings the next login would restore.
  - The chooser's `PINBOARD_ENDPOINT` call is answered when the *store* has
    spoken, not when the request was decoded, so the chooser still reports
    whether its document was actually published. A request the user's next
    gesture overtakes before any worker took it is answered there and then, so
    no caller is left parked on an answer nobody will produce.
- **The program catalogue is read off the loop too.** Two configuration
  documents and then one `AppInfo` per catalogued application is far more than a
  frame's worth of reads, and it used to happen on the very click that opened
  the launcher. The popup now opens on the catalogue already in hand and adopts
  the fresh one when it lands. The catalogue and the file-type associations
  arrive as *one* snapshot, because the associations are read from the bundles
  that very catalogue names — so a click can never resolve a bundle against a
  catalogue it was not read from. The one read that stays on the session's own
  task is the bring-up read, before any window is on screen and before anything
  can be clicked.

## Running-task list ↔ window stack

The taskbar models a running-task list — one entry per top-level window, with
the click-to-activate / minimise rule — but owns no window manager, and the
window manager owns no task list (`AGENTS.md` §17.4). `TaskBridge` is the glue
between them. A task is named by a `tairix_taskbar::TaskId` and a window by an
opaque `tairix_wm::WindowId`, so the bridge owns the correspondence: it mints a
stable task id per window it tracks and never reuses one, then translates
between the two whenever the bar acts on a window or the window manager moves
focus. Each operation is total and fails closed (`AGENTS.md` §2.9):

- `open` adds a window to the compositor, lists it as a running task, and shows,
  raises, and focuses it (a freshly opened window takes focus); it opens nothing
  only if the task-id space is exhausted;
- `close` removes the window from the compositor and its task from the bar,
  dropping focus if the closed window held it; an untracked window is a no-op;
- `raise` shows, raises, and focuses a window — what choosing a cell in the
  bar's hover picker asks for, and what a click on the slot of an application
  with a window to raise asks for — and is a no-op for an unknown window;
- `minimize` is the title bar's own control's counterpart: it marks the entry
  minimised, hides the window, and drops focus if it held it;
- `sync_focus` mirrors a window-manager focus change (the user clicked a window
  directly, or pressed the desktop) back into the bar's highlight, returning
  whether the highlight moved so a click on a window that owns no task neither
  blanks the highlight nor forces a needless repaint.

`DesktopShell` drives the bridge: `open_window` / `close_window` manage the
lifecycle, `handle` applies a `WindowChosen` outcome to the compositor and
mirrors a window-manager focus change into the bar, and the focus move uses the
window manager's new `InputRouter::focus` / `unfocus` (validated against the
compositor, fail-closed) so focusing a task by id keeps the keyboard owner in
step. The bridge holds no pixels and grants itself no authority — the
compositor, the router, and the taskbar are the embedder's, passed in per call.

## Driving the desktop from a live input stream

`DesktopShell` composes the four pieces above — the `DesktopSession`, the
`SessionInputRouter`, the `TaskbarPresenter`, and the `TaskbarRenderer` — into
one event-driven frontend, closing the long-open "feed the router and presenter
from live device events" thread. A real desktop runs a loop: read the pending
pointer events, route each, perform the session-level effect of a taskbar
action, and bring the on-screen bar back in step. `DesktopShell` runs exactly
that loop over an injected `InputSource` seam (a real pointer/keyboard channel
on a running system, an in-memory queue in tests, `AGENTS.md` §7):

- `pump(source, &mut Compositor, now_ns)` drains every pending event, applying
  each in order and returning a `ShellOutcome` per event. To avoid flooding an
  app with a dense gesture, an adjacent run of one gesture over one window is
  folded in the returned list: pointer motions collapse to the latest
  position, and wheel ticks in one direction sum into a single delta (a
  reversal ends the run, because a tick that clamps at a range end is not
  recovered by the tick back). Every sample still drives the window manager's
  own hover and drag state, so the folding is safe. Outcomes are `Ignored`, a
  `WindowManager` action the embedder may observe, or a `Taskbar` response.
  One drain is one instant: the embedder reads the monotonic clock once when
  the source wakes it and every event of that batch resolves its
  tap-versus-hold gesture against the same `now_ns`;
- a taskbar response is applied where the shell's own state suffices (a task
  activate/minimise outcome drives the compositor) and the bar is
  re-presented straight from the taskbar's drained per-surface repaint latch.
  Every model change that alters what a surface draws latches that surface, so
  an opened/closed popup, a fold, or a changed task highlight reaches the
  screen without double-painting, while a motion that crosses no control —
  over the desktop, over a window, or over dead space on the bar — repaints
  nothing at all;
- the frame is settled **once per drained batch, at one site**: one taskbar
  present, one active-frame sync, one cursor refresh, whether the batch held
  one event or sixteen. All three read *current* state rather than the event
  that changed it — the present repaints the union of the surfaces the batch
  latched, the sync compares the current focus against the shown active
  frame, and the cursor follows the pointer's latest position — so settling
  once leaves the desktop N settles would have left, and drops work nothing
  could observe, because the embedder publishes one frame per drain. A drain
  that found no event settles nothing, so an idle wake is free. `handle` is
  the single-event form of the same thing: apply, then settle;
- a faulting `InputSource` ends the `pump` with its `Errno`; the events drained
  before the fault stay applied and are settled, so the screen never shows a
  state the model has left, and the embedder replaces or re-polls the source.

The shell holds no framebuffer and grants itself no authority: the `Compositor`
is the embedder's and is passed in on each call. A loaded notification-icon set
is installed with `set_icons`, the merged catalog handed over with
`set_library`, a running app's window raised with `raise_window`, a title-bar
drag armed with `begin_move`, and the desktop torn down with `teardown`. A
response the shell cannot perform with its own state — launching an app,
opening the file manager — is surfaced as a `ShellOutcome` for the embedder,
which holds those capabilities (`AGENTS.md` §16.5).

## Live device input source

The `InputSource` the shell `pump`s is now backed by a live device channel.
`DeviceInputSource` (the `device` module) wraps an injected
`PointerInputChannel` seam — a capability-checked kernel input channel on a
running system, an in-memory queue in tests (`AGENTS.md` §7) — that hands the
desktop one framed `tairix_abi::input::PointerInput` record at a time. Each
`poll` reads one record and decodes it through `PointerInput::from_bytes` into
the `lib/input` `InputEvent` the window manager and taskbar route: a `MovedBy`
record's relative displacement is accumulated — saturating, clamped to the
screen rectangle the source is constructed with (an empty screen is refused
at construction), starting at the screen centre — into an absolute
`PointerMoved`, and a `Pressed` / `Released` record becomes a
`PointerPressed` / `PointerReleased` carrying the resolved `PointerButton`.
The accumulation lives here deliberately: the seat channel is
screen-independent, and only this seat-owning session knows the compositor's
pixel extent, so a driver never needs display-geometry authority and a
hostile injector can pin the pointer to an edge but never move it off-screen
or wrap it. The crate holds no input capability of its own — the channel
delivers the bytes and the decode runs above the device (`AGENTS.md` §17.4 /
§19.5) — and a malformed record fails closed with its `Errno` rather than being
misinterpreted, ending the shell's `pump` without disturbing the events already
applied (`AGENTS.md` §5.4 / §2.9). The ABI record itself is the seat-channel
pointer record documented in [Input events](../abi/input.md); it is a
distinct layer from the device-level driver input ABI, not a duplicate of it
(`AGENTS.md` §2.2).

## Live keyboard input source

The keyboard's live backing is `KeyboardInputSource` (the `keyboard` module),
the counterpart of `DeviceInputSource`. It wraps an injected `KeyInputChannel`
seam — a capability-checked kernel keyboard channel on a running system, an
in-memory queue in tests (`AGENTS.md` §7) — that hands the desktop one framed
`tairix_abi::input::KeyInput` record at a time. Each `poll` decodes one record
through `KeyInput::from_bytes` into the same `lib/input` `InputEvent` stream the
shell pumps: a `Pressed` / `Released` record becomes a `KeyPressed` /
`KeyReleased` carrying the resolved `Key` (a produced `Char`, or a `NamedKey` —
the wire ABI's twelve function-key codes fold into one `NamedKey::Function`)
and the held `Modifiers`. The `SessionInputRouter` routes a key to the window
manager, which delivers it to the focused window; the taskbar takes no keyboard
input. As with the pointer the crate holds no input capability of its own, and
a malformed record fails closed with its `Errno` rather than being
misinterpreted (`AGENTS.md` §5.4 / §2.9). The ABI record is documented in
[Input events](../abi/input.md).

## Seat-backed input channels

The `PointerInputChannel` and `KeyInputChannel` seams above are backed by the
kernel **seat registry**, not by IPC ports: `SeatInputChannel` (the `seat`
module) drains each fixed-width input record from the per-seat, owner-gated
channel the kernel routed the desktop's input to
([the seat page](./seat.md)). The records arrive through an injected
`SeatEventReader` seam — the seat-addressed
[`pointer_read` / `keyboard_read`](../architecture/syscalls.md) syscalls
(`tairix_rt::pointer_read` / `tairix_rt::keyboard_read`) on a running system,
an in-memory queue in tests (`AGENTS.md` §7) — so the crate holds no seat
lease of its own and stays host-testable (`AGENTS.md` §17.4).

The security property lives kernel-side: each drain is gated on
`CAP_INPUT_READ` **and** owner-gated against the seat's live lease, so only
the session that acquired the seat ever receives the stream — a named IPC
port was deliberately rejected for input, because a port's receive gate is
capability-only and cannot express "only the live seat-lease holder may
drain". The channel's own validation is narrow and fail-closed (`AGENTS.md`
§5.4 / §2.9): an empty drain is `None`, and a drain of anything other than
exactly one whole record (`WIRE_LEN` bytes) surfaces `LengthOutOfRange`
rather than handing truncated bytes to the decoder.

A pointer record and a key record are each a fixed-width drain from the
caller's own seat, so `SeatInputChannel` implements **both** seam traits
through one shared validation path rather than two (`AGENTS.md` §2.2); which
records flow is decided by the reader it wraps — a pointer reader wrapped in
`DeviceInputSource`, a keyboard reader in `KeyboardInputSource`.

Relaying a theme switch to the apps over IPC remains a later increment; the
desktop now reads a live pointer **and** keyboard event stream end to end,
each channel drained from the kernel's owner-gated seat channels.

## Giving memory back under pressure

The desktop holds the largest reclaimable allocations on a graphical system:
rasterised cursors and notification glyphs, decoded icon artwork, rendered
window furniture, and every window's content pixels. The kernel's
memory-pressure band is one more member of the session's wait set, so a
deepened band **wakes** the loop rather than being discovered by polling
(`AGENTS.md` §2.23); the woken branch runs one `DesktopShell::trim_caches`,
which is the single place the desktop's whole answer lives:

- the cursor, notification-glyph, icon-artwork, and window-furniture
  `ReclaimCache`s each shrink to their pressure-derived target, wiping what
  they release, and
- `Compositor::release_content_under_pressure` runs the content ladder for the
  window the session's own router currently focuses (see [Releasable window
  content](./wm.md#releasable-window-content)).

Releasing content is only half of the handshake: the session then drains
`Compositor::pending_redraws` and delivers a `WindowEvent::RedrawRequested` to
each released window's owning app over the window channel, mapping the
compositor's `WindowId` back to the client's window through the same served-
window table every other app-ward event uses — there is no second mapping. The
same drain runs after a window becomes visible again, so a restored window that
lost its pixels while minimised is asked for them immediately. Only windows the
embedder declared app-presented are ever released, so the bar, the lock screen,
the picker, and a confirmation prompt — which the session paints itself and no
client would redraw — keep their pixels at every band.

Logout or seat loss runs `teardown`, which tears every cache down and wipes
each window's content: a session's retained pixels never outlive the session.

### Read the band before building the caches

A reclaimable cache admits **nothing** while the reported band is the
fail-closed *unknown* it starts in — refusing to charge memory it has not
been told the machine can spare is the right default, and it is why the live
session asks for the band once *before* it constructs its caches rather than
only on the first pressure wake. Skip that first read and the desktop is
correct but permanently cold: every cursor, glyph, and icon lookup misses, so
the bar draws built-in glyphs for the whole life of a session on a machine
with memory to spare. The later read, after the wait-set member is
registered, is a different job — it closes the race with a band that changed
while the session was still coming up, because the kernel reports *changes*.

## Tests

`cargo test -p tairix-desktop-session` covers: the default dark start with an
empty library and a closed popup; `set_theme` relaying the new metrics to the
taskbar (observed through a custom theme with a distinctive corner radius);
the fail-closed `UnknownTheme`/`DuplicateId` paths leaving the taskbar
untouched; and `TaskbarPresenter` placing and rounding the bar, reusing its
window across presents, showing the popup and menu while they are open,
re-creating a window an embedder removed, and `teardown` clearing every window.
It also covers `SessionInputRouter` against a **presented** bar, because that
is what the seat resolves against: presses on the bar's buttons and slots
routing to the taskbar while a press over a window or the empty desktop routes
to the window manager; a press (either button) where a window covers the bar
reaching that window rather than the control beneath it, with the buttons that
window does not cover still claimed by the bar, and the bar still winning over
a window stacked beneath it; a scroll over a covered capsule reaching that
window; modal surface modality (claiming off-panel presses and keys); a window
drag continuing while the pointer is over the bar, and a refresh never
interrupting it; the implicit grab keeping a gesture where it started —
including across a chord, where it ends at the *last* button — and handing the
pointer back to whatever is under it afterwards; a pointer resting on the bar
not diverting a keystroke from the focused window; a capsule tap and hold each
asking the session to open the Switchboard at their own section; and a release
the bar does not claim ending the grab.

It covers the pointer's focus, which is what makes hover honest: a window
raised over a hovered application slot taking the hover with it and closing the
window picker (with the pointer never moving), the pointer moving off the bar
onto a window doing the same, a covering window closing handing the hover back
without a motion *and* deliberately not re-opening the picker, a window raised
over the Switchboard capsule collapsing its readout, and the pointer crossing
onto the bar putting out a window's title-bar command.

It covers the desktop's reveal from black: the fade walking the theme's own
session-fade span frame by frame to a fully revealed screen and then asking
for nothing; an idle desktop — and an unengaged lock — leaving the park
exactly the value it was handed; reduced motion revealing at once with no
repaint and no timer; a display refusing a present mid-fade still reaching a
fully revealed desktop; and a refused unlock animating on the session's
clock, shortening the park while it runs and returning it untouched once it
settles.

It covers the library loader (`load_library`): absent stores silent and empty,
parsed stores, the user overlay's per-field override winning, and malformed /
oversized / non-UTF-8 stores each yielding an empty catalog plus one warning
line. It covers the icon bar's service: windows grouped under the process that
owns them rather than one slot per window, a declaration holding a slot with
no windows and leaving on withdrawal, a window alone holding a slot with no
menu and a raising click, a slot keeping its place while it lives, the
strip's bound refusing a further declaration, the manifest-attested identity
(and a bundle with no readable manifest stating only a name, and a process the
desktop did not launch stating not even a version), one manifest read per
bundle with the identity forgotten when nothing runs from it, each slot
carrying only its own process's declaration, and a bundle whose manifest
presents no slot being off the strip by either route — while a manifest that
is absent or will not decode never gives a slot up. It covers the picker's cells (one
per window, captioned, a window with no frame yet carrying no thumbnail, and
the refusal below a choice) and the thumbnail scaler itself. It covers the
shared artwork store (rasterised artwork reaching a slot, refusals cached,
verified pixel shape, one read and one decode per asset and side, the budget
honoured, and a trim then a teardown giving the pixels back). It covers the
library
popup's per-row icons: a row drawing its own bundle's icon, an absent /
over-long / undecodable asset and an entry declaring no icon all falling back
to the shipped app-bundle artwork rather than blanking, only the rows the
viewport shows being resolved at all, and — with a reader that serves nothing
whatsoever — the bar and popup drawing pixel-for-pixel what the glyph-only
desktop draws.

It covers `DesktopShell`: `pump` opening the popup from a press on the Library
button and presenting it, `set_library`/`set_apps` handing the catalog and the
strip over and refreshing open views, the full launch flow, `raise_window`
restoring and focusing a minimised window, hover latching a present of the
bar, fault propagation, `begin_move` arming a grab, `set_icons` installing a
loaded set, and `teardown`. It covers the window host relaying a declaration
and its withdrawal, a secondary press over a slot opening the declared menu as
its own window (and a click away taking only that down), and the hover picker
becoming its own window and its cell choice raising the window it names. And
it covers `TaskBridge` end to end through the shell: `open_window` listing,
focusing, and presenting a new window; `close_window` removing it and dropping
focus; the title bar's minimise hiding it and a raise bringing it back; and
syncing focus to an untracked window.

It covers the Switchboard channel through `serve_switchboard_request` and its
fake seams: a successful publish answering with the session's own `ProcId`
while every refusal stays the plain status frame; an unattested caller refused
on all three requests with the model untouched; `ActivateOwner` raising the
right window and an unknown owner refused `NotFound`; `RestartOwner`
re-launching the recorded bundle and an unknown owner refused `NotFound`; a
malformed frame refused; a capsule gesture sending exactly one `OpenPanel` at
the section the bar chose; a press refused by a still-starting instance held
and opened on its first publish; a pending open the mailbox refuses put back
for the next one; a confirmed power transition the holder refuses reported
rather than passing for success; a pending open delivered on the next successful
publish and never re-sent; a seat report sent only when the unresponsive set
changes, carrying the truthful total when more owners are hung than the frame
can name; and a refused mailbox send attempted exactly once. The `vigil`
tests cover the flagging threshold, the clearing delivery, reap forgetting,
and the bounded `unresponsive_owners` enumeration.

The `launch` module's own tests cover the reserved-status reporting table, the
run-path-keyed `LaunchTable`, and the shared reap flow. It also covers
`DeviceInputSource`: accumulating `MovedBy` displacements into absolute,
screen-clamped `PointerMoved` positions, each pointer button for press and
release, a malformed record surfacing `BadMagic`, a channel fault propagating,
and `into_channel`. It covers `KeyboardInputSource` likewise: decoding a
character press with its modifiers, a named release, a malformed record
surfacing `BadMagic`, a channel fault propagating, and `into_channel`. Finally
it covers `SeatInputChannel`: draining a pointer move, a pointer press, and a
key press from the in-memory seat reader; a drained channel yielding `None`;
a one-shot reader fault propagating then recovering; and the fail-closed
paths — a partial record refused as `LengthOutOfRange` and a whole-length but
structurally invalid record surfacing `BadMagic`.

`pace_tests.rs` covers the frame deadline: the first damaged frame admitted at
once and a whole flood inside one period costing that one frame; a sustained
flood costing no more than one composite per period while still getting its
frames; a held frame arming exactly the time left of its period, never a
zero-length deadline, and folding the park back to indefinite once it is
admitted; a park already shorter left alone; an idle session and every
undamaged frame arming nothing and not starting the period; an interaction
slower than the period never held; an animation's cadence frames never
deferred; and a clock jumped backwards — or a long background spell —
admitting rather than stalling. Beside the frame-cost tests, the pacer is
driven as the serve loop drives it: sixteen pointer samples pumped through the
real shell and compositor inside one period, each moving the cursor and so
really damaging the screen, composite *nothing* until the deadline it armed.

`desktop_tests.rs` covers the `Desktop` model: hover feedback and
click-on-empty-desktop clearing the selection; the shared double-click
tracker activating an icon on its second press; keyboard arrows moving the
selection by one icon and by one whole column, wrapping at the ends, `Enter`
activating, and `Escape` clearing; every activation branch (a directory
opening the file manager at its path, a bundle launching, a plain file
resolving its association and launching with the file as its argument, and
an unassociated file refused with a stated reason); the rate-limited
pointer-arrival re-list (due, not yet due, and a re-list the source refuses
leaving the listing exactly as it was); and the selection following a
renamed-or-reordered entry by name across a re-list rather than by index.
