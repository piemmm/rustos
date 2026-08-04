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

## Pinned shortcuts

The session owns the user's list of **pinned shortcuts** (`plans/NEW-TASKBAR.md`
T6), stored as per-user configuration at `~/Settings/Taskbar/pins.conf` (the
[`tairix-taskpins`](../lib/taskpins.md) store). It loads them with the same
fail-closed posture as the library: an **absent** store is silently empty, and
an **unusable** one contributes an empty list plus a loud `stderr` warning.

The session is the store's only writer. Every edit (pin, unpin, or a
drag-and-drop insertion) rewrites the document whole through the one
`SessionFileWriter` seam — the write-side twin of the reader — and the
in-memory list adopts the edit **only after the write succeeded**, so memory
and disk never diverge. A refused write leaves the bar exactly as it was,
with a diagnostic reported on `stderr`.

### Resolution and icon pipeline

Resolution turns each stored `PinTarget` into the view the bar renders:

- an **`entry`** pin resolves through the merged program-library catalog (name,
  icon asset, bundle);
- a **`bundle`** pin resolves through its own bounded, fail-closed `AppInfo`
  manifest read;
- an **unresolvable** pin (e.g. an uncatalogued entry) keeps a best-effort
  identity with no launch path, so it can still be seen and unpinned.

Bundle icon bytes (SVG or PNG) are **untrusted third-party input**, so the
session never decodes them in its own address space. Instead, they go to the
**parser-sandbox icon-rasterisation service**: the session's own binary
re-entered as a capability-empty worker ([the sandbox
page](../security/sandbox.md)). The rasterised RGBA pixels are verified and
cached per `(asset path, pixel side)`, including refusals; a missing or bad
icon falls back to the shipped application-bundle artwork and then to the
shared application-class glyph. `Taskbar::pin_icon_side` exposes the exact
geometry so the session rasterises artwork at the drawn size.

Running-window matches are recomputed cheaply each loop wake from the attested
launch table and window ownership records, never from window titles.

### One artwork store for the whole desktop

The pins are not the only thing on the bar with an icon, so the read, the
sandboxed decode, and the cache are **not** the pin module's private
machinery: they are the shared two-tier artwork layer (`lib/icon`'s
`ArtworkCache` plus its `ArtworkReader` / `ArtworkRasteriser` seams), and the
`DesktopShell` owns exactly one of each for the seat (`AGENTS.md` §2.2).

- `DesktopShell::set_artwork_source(reader, rasteriser)` installs the live
  seams — on a running system the VFS reader and the sandbox worker, in tests
  a pair of fakes. A shell that is never given them starts with seams that
  find and decode nothing, so a bare shell draws built-in glyphs rather than
  failing.
- `DesktopShell::artwork_parts` hands out the cache and both seams together,
  which is how the embedder resolves pin views without borrowing the shell
  three times.
- The same cache answers the bar's `IconArtwork` lookup (through
  `IconArtworkSource`) for the shipped raster masters under
  `/System/Graphics/Icons`, so the launcher buttons, a pin, a running task,
  and a library row all draw out of one store and one budget.

Every lookup is keyed by (what was resolved, pixel side), and a refusal is cached
like a success, so an application whose icon will not decode costs one read
and one sandbox round trip — not one per frame.

### The library popup's per-row icons

A program-library row shows its own application's icon through exactly the
pin resolution above, driven off what the popup says it is showing:
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

### Pin service and window-channel bridge

`PinService` manages the live store, the one armed drag offer, and a dirty
latch the loop drains to re-resolve views before its next present. It
implements the window-channel bridge (`PinBridge`) through which apps ask to
be pinned: a `PinBundle` request is validated (store-shaped path, decodable
manifest) and applied fail-closed.

A drag can start from either of two origins, named by `DragOrigin`: a served
application window (`Window`, offering its own bundle path over the window
channel) or the taskbar's program-library popup (`Library`, offering a
catalogued entry it is dragged out of — see [Dragging a library entry to pin
it](taskbar.md#dragging-a-library-entry-to-pin-it)). `offer_drag` /
`withdraw_drag` arm and disarm the one live `DragOffer`, and `take_drag_for`
consumes it only for the origin that armed it, so a release from one origin
can never claim or withdraw a drag another origin started. `resolve_pin_drop`
resolves the gesture: a primary release from the offering origin over the pin
band re-validates the target through `pin_target_at` — a bundle path against
its manifest, a library entry against the live catalog, so an application
uninstalled between the drag and the drop is refused rather than pinned
unlaunchable — and pins at the drop index; the offer is consumed either way.

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
only a hovered, selected, or focused icon paints a panel behind itself.

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

**Why the desktop is not a pin-drag source.** An installed application lives
only in an application store — machine-wide, or the user's own — so a
`.app` directory a user drops on their `Desktop` folder is a directory
*shaped like* an application rather than an installed one, and
`BundlePath`'s store rule correctly refuses it. Offering a pin gesture that
could never succeed would be a promise the system cannot keep, not a
feature; the pin drag source is the program-library popup instead (see *Pin
service and window-channel bridge*, above), whose every row is a catalogued
entry by construction.

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

`DesktopShell::present_desktop` repaints the layer **in place**, through
`Compositor::repaint_desktop`, into the screen-sized buffer the compositor
already holds: a hover, a moved selection, or a re-list repaints the desktop
often, and a whole screen of pixels is not something to re-allocate per frame.
The layer is opaque and covers the screen. Its base is the wallpaper the
embedder prepares and installs with `DesktopShell::set_wallpaper` (it holds the
capability to read the user's chosen image and the sandbox that decodes it, and
it fits the pixels through the one shared placement in `lib/wallpaper`); the
shell blits what it is handed and parses nothing. The backdrop colour the
settings name — the active theme's own desktop colour for `Backdrop::Theme`, the
chosen flat colour for `Backdrop::Colour` — is laid down wherever the wallpaper
does not reach, which with no wallpaper at all is everywhere. The icons are then
drawn over that base exactly as before, in the work area, so nothing is ever
drawn under the taskbar. A layer the heap will not give back leaves the desktop
exactly as it was rather than blanking it.

### The pinboard's context menu

A secondary press on the backdrop is the desktop's context-menu gesture:
`Desktop::context_press` selects the icon it landed on (so the menu acts on what
the user pointed at) or, on empty backdrop, leaves the selection exactly as it
was, and asks for `DesktopAction::OpenMenu { at, on_icon }`. It claims no
keyboard focus, because the window manager does not move focus for a secondary
backdrop press (`InputResponse::DesktopSecondaryPressed`) and the desktop does
not pretend otherwise.

`PinboardMenu` (`userland/gui/session::pinboard`) is that menu: one shared
`lib/controls` `Menu` plus the anchor it was opened at. Its command set is
closed (`plans/PINBOARD.md` §7) — *Open* (only when the press landed on an
icon), *New Folder*, the four *Sort by* rows, the two *Arrange from* rows,
*Refresh*, *Open Desktop Folder*, and *Change Background…* — and one `rows_for`
pass builds each row together with the `PinboardCommand` it names, so a row
index can never disagree with the command it dispatches. The sort order and
arrangement already in force are drawn marked and non-actionable with their
reason stated, because choosing what is already in force is a statement of
where the desktop is rather than a command. `PinboardMenu::layout` anchors the
plate at the pointer and clamps it wholly onto the screen, so a right-click in a
corner opens a menu the user can reach; a closed menu has no plate at all
rather than a fabricated one at the origin.

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

`DesktopShell::present_pinboard_menu` shows the open plate as its own
compositor window and takes that window down when the menu closes, through the
same shared `place` helper the taskbar's own menu window uses, rounded with the
same popup radius the plate is painted with. A plate surface the heap will not
give back leaves what is on screen untouched rather than showing an empty
window.

## Resolving taskbar responses

A `tairix_taskbar::TaskbarResponse` flows out of `DesktopShell::handle` as a
`ShellOutcome::Taskbar` value. The shell applies what its own state suffices
for — a `TaskActivated` outcome drives the compositor through the
`TaskBridge`, popup-internal changes just re-present — and the embedder (the
`Run` binary) performs what needs capabilities the shell does not hold
(`AGENTS.md` §10, §16.5):

- `OpenFiles` — the permanent Files button. The embedder opens the file
  manager **idempotently**: if a desktop-launched file manager is already
  running and serving a window, that window is raised and focused
  (`DesktopShell::raise_window`); if its launch is still in flight, the
  press is already satisfied; only otherwise is the bundle spawned.
- `ActivatePin { index }` — an idempotent launch-or-raise of the pin at
  `index`, using the same rule as the Files button (the shared
  `activate_bundle`).
- `Unpin { index }` and `PinEntry { entry }` — edits the pin store and sets
  the dirty latch; a refused edit is reported on `stderr`.
- `LibraryLaunch { entry }` — a chosen library entry, resolved through the
  catalog and spawned (see *The program library*).
- `OpenLibrary` — the popup opened; the embedder re-reads the stores so the
  listing is current.
- `LibraryDismissed`, `NotificationActivated`, `ClockPressed` — surfaced for
  the embedder; the bar's own state is already up to date.

The bar's **context menu** is presented by the presenter as its own small
rounded window (a third window beside the bar and popup).

The capsule's system quick actions (`plans/NEW-TASKBAR.md` T13) arrive as
the same kind of typed outcome — the bar decides nothing and holds no
authority for any of them:

- `OpenSwitchboard { section }` — both the capsule's own press *and* the
  menu's two inspection rows (*About This System* → `Overview`, *System
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

## Launch bookkeeping

The `launch` module owns the desktop's launched children. `LaunchTable`
remembers every child still running — its PID, the display label
diagnostics report it by, and the `Run` path it was spawned from (its
**attested bundle identity**: the desktop spawned the child itself, so no
window title or other app-controlled data is ever trusted for it,
`AGENTS.md` §23.1). `running_from` resolves the Files button's idempotent
open; `window_of_pid` (in the `Run` binary) finds the running app's served
window through the window engine's kernel-attested ownership records.
Asynchronous launch admits a child and returns its PID before the image
loads, so a load refusal surfaces as the child's reserved `LOAD_*` exit
status: the shared `reap_launched` drains every exited child in one wake,
reports each refusal on `stderr` named by its label (`launch_failure_report`
— fail loud, never fatal, `AGENTS.md` §2.24), tears down the child's
windows, and forgets the table entry.

## The Switchboard tray feed and hang detection

The taskbar's right-most Switchboard capsule renders live state from two
independent, honest feeds (`plans/NEW-TASKBAR.md` T9/T10):

- **The published summary.** The `Run` binary spawns the Switchboard monitor
  service (`/System/Services/switchboard.app`, `SWITCHBOARD_RUN_PATH`) at
  bring-up as the logged-in user and binds the seat-scoped
  `SWITCHBOARD_ENDPOINT` beside the window and notification rendezvous. Each
  publish is attested: the caller's kernel-provided `call_peer_origin` pid
  must match the launch table's live entry for the service's bundle path — a
  foreign process, an orphan of an earlier session, or a hand-launched copy
  is a typed refusal stated on `stderr`, never rendered (`AGENTS.md` §5.4).
  The decoded `TraySummary` reaches the capsule through
  `DesktopShell::set_tray_summary`; when the service exits, the reap path
  clears the feed (`set_tray_summary(None)`) so the capsule falls back to
  calm rather than freezing a dead service's last summary.
- **The session's own delivery evidence.** The desktop is the one component
  that observes whether an app drains its window events: every app-ward
  event is a non-blocking mailbox send. To avoid flooding an app with samples
  it can only act on the newest of, `pump` coalesces adjacent pointer motions
  over one window into the latest position, while ensuring every sample still
  drives the window manager's own hover and drag state. The production event
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
the launch table's live entry) and then validated against what this session
can actually see (`AGENTS.md` §5.4):

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
confused for one another. Two commands travel it:

- **`OpenPanel { section }`** — the capsule gesture. The bar decides the
  section (a quick press its running-task list, a press held past the bar's
  long-press threshold its recovery list) and emits it as
  `TaskbarResponse::OpenSwitchboard { section }`; the session only maps that
  choice onto the wire vocabulary, so the section a user asked for is never
  re-decided a second time. A press is **never lost**, whatever state the
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
- the bar's context menu, the notification popover, and the Switchboard
  capsule's instrument readout are presented the same way while each is
  open (`MenuLayout` / `NotificationsLayout` / `TrayReadoutLayout`), and
  each window is removed the moment its surface closes.

`present` repaints **only the surfaces the taskbar latched as changed**. It
takes the `tairix_taskbar::TaskbarRepaint` that `DesktopShell::present`
drains from the model, and each of the five surfaces above is re-rendered
and re-pushed only when its flag is set. This is what makes hovering cheap:
each surface costs a full re-render and marks its whole window rectangle
dirty, so a pointer crossing one small open menu must repaint that menu
alone. Two things override an empty latch — a surface that has no window yet
is always painted, so the first frame puts everything on screen, and a
change of desktop density repaints everything, because the scale belongs to
the output rather than to the taskbar model the latch tracks.

The presenter owns only the compositor `WindowId` tokens it minted — the
taskbar model, the renderer, and the compositor are the embedder's, so the
session composes the GUI crates without owning the window-manager handle. It is
total and fails closed (`AGENTS.md` §2.9): a render that cannot allocate its
surface leaves the on-screen window untouched rather than blanking the bar, a
window the compositor no longer knows is re-created on the next present, and
`teardown` removes every window so a session shutdown leaves nothing orphaned.

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

Whether the row is offered at all is the session's attestation: it tells the
bar through `DesktopShell::set_lock_available` whether it runs on a console
that has an elevation endpoint to unlock with. The bar refuses the row until
told otherwise, because a lock that could never be undone would strand the
user rather than protect them.

## Routing one input stream to both routers

The desktop has two input routers — the window manager's `tairix_wm::InputRouter`
(focus, click-to-activate, interactive move-grabs) and the taskbar's
`tairix_taskbar::TaskbarInput` (the launcher buttons, the program-library
popup, task activate/minimise, notification/clock presses) — and both consume
the **same** shared `tairix_input` event vocabulary (`AGENTS.md` §17.4, §2.2).
A real input source produces one event stream, so `SessionInputRouter` is the
glue that fans it to the right router, driven through
`handle(event, &mut Compositor, &mut Taskbar, now_ns)`. The monotonic
`now_ns` is threaded down because one taskbar gesture is decided by *time*:
the Switchboard capsule distinguishes a tap from a hold by how long its
press has been down when the next event arrives, so the bar is given the
embedder's clock reading rather than reading a clock of its own — the same
instant every router sees, and the one an in-memory test controls:

- while the bar's **context menu** OR the **program-library popup** is open it
  is modal: every press (any button), release, scroll, and key event routes
  to the taskbar. Motion is still *tracked* by the window manager so its
  pointer stays in step for the moment the surface closes, but its outcome is
  discarded — nothing is delivered to the windows beneath a modal surface;
- otherwise a **press** goes to the taskbar iff the pointer is over the bar
  (a secondary press there opens a pin's context menu; a middle press over
  the Switchboard capsule switches to the previous task) or over one of its
  open non-modal popovers — the notification popover and the capsule's
  instrument readout — and a remaining primary or secondary press goes to
  the window manager: the two never both act on one press;
- a **scroll** over the Switchboard capsule or its open readout routes to
  the taskbar (it cycles the running tasks); every other scroll goes to the
  window manager's viewport under the pointer;
- **pointer motion** is fanned to both so their tracked pointer positions stay
  in step; the window manager acts on it (dragging a grabbed window) and the
  taskbar refreshes its hover feedback. Motion is also where a capsule press
  held past its long-press threshold resolves, and that is a real action: it
  takes the outcome while the drag still applied;
- a **primary release** goes to the taskbar *first* — a quick press on the
  Switchboard capsule resolves on its release — and one the bar does not
  claim ends an in-flight move-grab in the window manager instead;
- **key events** go to the window manager — which delivers them to the
  focused window — except while a modal surface is open (above);
- a middle press away from the bar, a non-primary release, or a press/motion
  neither router acted on, is `SessionInputResponse::Ignored`.

Decorations start a title-bar drag through `begin_move`, and the embedder reads
the keyboard owner through `focused`. The router holds no pixels and grants
itself no authority; every routed sub-call is itself total and fails closed
(`AGENTS.md` §2.9).

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
- `activate` applies the bar's `ActivateOutcome` to the compositor — an
  activated task is shown, raised, and focused; a minimised one is hidden and,
  if it held focus, unfocused — and is a no-op for an unknown task;
- `sync_focus` mirrors a window-manager focus change (the user clicked a window
  directly, or pressed the desktop) back into the bar's highlight, returning
  whether the highlight moved so a click on a window that owns no task neither
  blanks the highlight nor forces a needless repaint.

`DesktopShell` drives the bridge: `open_window` / `close_window` manage the
lifecycle, `handle` applies a `TaskActivated` outcome to the compositor and
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

- `pump(source, &mut Compositor, now_ns)` drains every pending event, routing
  each through `handle(event, &mut Compositor, now_ns)` and returning a
  `ShellOutcome` per event. To avoid flooding an app with samples it can only
  act on the newest of, adjacent pointer motions over one window are
  coalesced into the latest position in the returned list; every sample still
  drives the window manager's own hover and drag state, so the coalescing
  is safe. Outcomes are `Ignored`, a `WindowManager` action the embedder
  may observe, or a `Taskbar` response. One drain is one instant: the
  embedder reads the monotonic clock once when the source wakes it and every
  event of that batch resolves its tap-versus-hold gesture against the same
  `now_ns`;
- a taskbar response is applied where the shell's own state suffices (a task
  activate/minimise outcome drives the compositor) and the bar is
  re-presented — **exactly once per event, at one site**, straight from the
  taskbar's drained per-surface repaint latch. Every model change that
  alters what a surface draws latches that surface, so an opened/closed
  popup, a fold, or a changed task highlight reaches the screen without
  double-painting, while a motion that crosses no control — over the
  desktop, over a window, or over dead space on the bar — repaints nothing
  at all;
- a faulting `InputSource` ends the `pump` with its `Errno`; the events drained
  before the fault stay applied and the embedder replaces or re-polls the
  source (`AGENTS.md` §2.9 / §19.5).

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
It also covers `SessionInputRouter`: presses on the bar's buttons and slots
routing to the taskbar while a press over a window or the empty desktop routes
to the window manager; modal surface modality (claiming off-panel presses and
keys); motion keeping the pointer in step; a window drag continuing while the
pointer is over the bar; a capsule tap and hold each asking the session to
open the Switchboard at their own section; and a release the bar does not
claim ending the grab.

It covers the library loader (`load_library`) and pin loader (`SessionPins`):
absent stores silent and empty, parsed stores, the user overlay's per-field
override winning, and malformed / oversized / non-UTF-8 stores each yielding an
empty catalog/list plus one warning line. It covers `SessionPins` persistence
and the refusing writer (memory and disk stay in step). It covers pin
resolution (catalog, manifest, and unresolvable fallback) and the shared
artwork store (rasterised artwork reaching a pin, refusals cached, verified
pixel shape, one read and one decode per asset and side, the budget honoured,
and a trim then a teardown giving the pixels back). It covers the library
popup's per-row icons: a row drawing its own bundle's icon, an absent /
over-long / undecodable asset and an entry declaring no icon all falling back
to the shipped app-bundle artwork rather than blanking, only the rows the
viewport shows being resolved at all, and — with a reader that serves nothing
whatsoever — the bar and popup drawing pixel-for-pixel what the glyph-only
desktop draws.

It covers `DesktopShell`: `pump` opening the popup from a press on the Library
button and presenting it, `set_library`/`set_pins` handing catalog/pins over
and refreshing open views, the full launch flow, `raise_window` restoring and
focusing a minimised task, hover latching a present of the bar, fault
propagation, `begin_move` arming a grab, `set_icons` installing a loaded set,
and `teardown`. It covers `PinService` decisions, drag management for both
`DragOrigin` values (a release from the origin that armed the offer
consuming it, a release from the other origin claiming nothing), the
`pin_target_at` admission for a bundle and for a catalogued entry, the
`resolve_pin_drop` policy, and `TaskBridge` end to end through the shell:
`open_window` listing, focusing, and presenting a new task; `close_window`
removing the task and dropping focus; clicking a task slot minimising/restoring
the window; and syncing focus to an untracked window.

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
