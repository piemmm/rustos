# tairix-desktop-session

The TAIRiX desktop **session glue** (`AGENTS.md` §10, `PLAN.md` Stage 7): the
component that owns the shared theme registry and the taskbar model and ties
the desktop's parts together. The crate also ships the `Run` binary of the
`desktop` **application** (`desktop.app` in the system application store,
`AGENTS.md` §16.8): a shell user starts the desktop by typing `desktop` — the
application store is on the fixed lookup prefix, so the bare word resolves
without any `PATH` entry — and the default graphical login
(`os.loginType graphical`) spawns the same bundle directly after
authentication — one bundle, one spelling. The command's grammar is closed
(`src/cli.rs`): bare `desktop` starts the session, the reserved `-h`/`-?`
switches serve its own `Help/` documents, anything else is a usage error.

The taskbar deliberately owns no theme registry, no filesystem reach, and no
spawn capability: its buttons and its program-library popup only *report*
typed `TaskbarResponse`s (open the library, open the file manager, launch
this catalog entry, relay an application's own icon-bar click or menu row).
Acting on those is the session glue's job — this crate owns the theme
registry and the icon bar's application strip,
loads and merges the program-library catalog, and resolves the responses
the shell's own state suffices for, surfacing the rest to the
capability-holding `Run` binary.

## What this crate owns

- **The shared `tairix-theme` `ThemeRegistry`** — the one runtime registry the
  whole desktop reads its theme from (`AGENTS.md` §6, §10).
- **The icon bar's application strip** — the `apps` module's `AppBarService`
  holds every application's icon-bar declaration as the window engine
  attested it, groups each live served window under the process that owns it,
  and resolves each slot's label, icon, and information-panel identity from
  the **signed** `AppInfo` of the bundle the desktop launched that process
  from. The strip is derived from live state on every wake it can have
  changed; nothing about it is stored.
- **The `tairix-taskbar` `Taskbar` model** — so a theme switch is a single
  in-place operation: the registry's active theme changes and the taskbar is
  re-themed to match (`DesktopSession::set_theme`; the interactive light/dark
  switch arrives with the Switchboard's System menu, `plans/NEW-TASKBAR.md`
  T13). `set_theme` fails closed on an unknown id and `register_theme` on a
  duplicate id, leaving the active theme and the taskbar untouched
  (`AGENTS.md` §5.4 / §2.9).

## The program library

The taskbar's popup lists the **resolved** program library
(`plans/NEW-TASKBAR.md` T5): the machine store
(`/System/Settings/ProgramLibrary/library.conf`) merged with the logged-in
user's overlay through the one `tairix_proglib::merge`. Reading those
documents needs a filesystem capability, so the `library` module's
`load_library` does it here, through the same `SessionFileReader` seam the
asset loaders use: an absent store is the ordinary empty state (silent), and
an unreadable, oversized, non-UTF-8, or malformed store contributes an empty
catalog **plus a ready-to-print warning line** — the desktop degrades to a
calm empty library and says why on `stderr`, never guessing at a half-parsed
store (`AGENTS.md` §2.24, §5.4). The merged catalog is handed to the popup
with `DesktopShell::set_library`; a `LibraryLaunch { entry }` response is
resolved back through that catalog to the entry's bundle `Run` path.

Both things an entry row can ask for go through the one `catalogued` lookup, so
a row can never act on two different bundles. The second is
`CreateDesktopShortcut { entry }` (`plans/SYMLINKS.md` S5):
`Desktop::shortcut_to` names a symbolic link in the user's own `Desktop`
folder after the entry's **display name** and points it at the bundle
directory, stored verbatim — the target's own leaf is what makes the shortcut
read as an application. A display name the one shared
`tairix_path::validate_file_name` refuses is refused with that rule's reason,
and a name already taken is the kernel's `AlreadyExists`: the link replaces
nothing, and nothing here works a collision around.

## The icon bar and the icon pipeline

`AppBarService` (`apps.rs`) holds every application's icon-bar declaration as
the window engine attested it (through the `AppBarBridge` seam the window host
borrows), groups each live served window under the process that owns it, and
resolves each slot's label, icon, and information-panel identity from the
**signed** `AppInfo` of the bundle the desktop launched that process from — so
an application cannot state an identity that is not its own. A declaring
application keeps its slot for the life of its process; one that declared
nothing but owns a window gets a slot with no menu, so no window is
unreachable. `picker_cells` answers a hover with one cell per window, each
carrying that window's last presented frame scaled through
`lib/raster::resample`.

Bundle icon bytes are untrusted third-party input, so the session never
decodes them in-process: they go to the **parser-sandbox icon-rasterisation
service** (the session's own binary re-entered as a capability-empty worker),
come back as verified RGBA pixels, and are cached per `(path, pixel-side)`.

That read, decode, and cache are the shared artwork layer's, not this
module's: `DesktopShell` owns exactly one `tairix_icon::ArtworkCache` for the
seat plus the one `ArtworkResolver` its misses are produced through, and every
icon on the bar comes out of it — an application slot, a picker cell, a
program-library row, and the shipped raster masters behind the two launcher
buttons (`AGENTS.md` §2.2).

- `set_artwork_resolver(resolver)` installs the live one. On a running desktop
  that is the icon-decoder thread over `ArtworkDesk` (`artwork.rs`), so a read
  and a sandbox round trip never happen inside a paint; where the kernel grants
  no thread it is `InlineArtwork` over the thin `ArtworkFileReader` /
  `ArtworkSandbox` wrappers, which adapt the session's own `SessionFileReader`
  and `IconRasteriser`, so there is one reading seam and one sandbox, not two.
- A shell that is never given one keeps a resolver that finds and decodes
  nothing, so a bare shell is a working desktop drawn from built-in glyphs.
- `artwork_parts()` lends the cache and the resolver in one borrow — how
  `build_pin_views` resolves a strip without borrowing the shell twice.
- `warm_icon_artwork` and `warm_launched_artwork` start the decodes the bar's
  surfaces and a launching application's windows will need, at the moment the
  catalog, the application strip, or the launch table naming them changes.
  Without them
  a decoder thread only moves the wait off the loop; with them the wait is over
  before the surface that draws the icon is shown.
- `trim_caches` shrinks it under memory pressure and `teardown` wipes it, on
  the same path as the cursor and glyph caches.

`resolve_library_icons` gives the program-library popup the same treatment,
driven off the rows the popup says it is showing: it asks for the visible
rows' icon requests, resolves each row's bundle icon at that row's own pixel
side, and files the answer back with `set_row_artwork`. A row whose entry
declares no icon — or whose asset is absent, oversize, or undecodable — falls
back to the shipped `AppBundle` artwork and then to the row's built-in glyph,
so a row is never blank. It runs at the top of `present`, before the paint,
skips a row that already holds right-sized artwork, and does nothing while
the popup is closed. Filing artwork deliberately does **not** latch a
repaint: latching from inside the pre-paint resolution would demand another
present every frame, forever.

**Read the pressure band before building the caches.** A reclaimable cache
admits nothing while the reported band is the fail-closed *unknown* it starts
in, so the `Run` binary asks for the band once *before* it constructs the
shell's caches. Skip that and the desktop is correct but permanently cold —
every cursor, glyph, and icon lookup misses and the bar draws built-in glyphs
for the whole life of the session on a machine with memory to spare. The
second read, after the pressure wait-set member is registered, is a different
job: it closes the race with a band that changed during bring-up, because the
kernel reports *changes*.

## Launch bookkeeping

The `launch` module tracks the desktop's launched children. `LaunchTable`
remembers each running child's PID, display label, and spawn path (its
**attested bundle identity** — the desktop spawned it, so no app-controlled
data is trusted); `running_from` resolves the Files button's idempotent open
(raise the running file manager instead of spawning a second copy).
Asynchronous launch surfaces a load refusal as the child's reserved `LOAD_*`
exit status, so the shared `reap_launched` drains every exited child in one
wake, reports each refusal loudly on `stderr` named by its label
(`launch_failure_report` — never fatal, `AGENTS.md` §2.24), tears the
child's windows down, and forgets the entry.

## Loading the on-disk graphics assets

The desktop's cursors and notification icons are authored as SVG under
`/System/Graphics` (the SVG-first asset rule, `AGENTS.md` §10 / §16.2).
`lib/cursor` and `lib/icon` own the decode-and-fall-back logic but stay
`no_std` with no path of their own; reading the bytes needs a filesystem
capability, so it is the session's job (`AGENTS.md` §17.4 / §19.5). The
`assets` module is that job:

- A caller supplies a `SessionFileReader` (the session's one file-reading
  seam, shared with the catalog loader; VFS-backed on a running system, an
  in-memory table in tests).
- `DesktopSession::load_cursors` reads one asset per cursor kind named by the
  active theme's `CursorSet`, from
  `/System/Graphics/Cursors/<asset-id>.svg`, and returns a `CursorTheme` the
  window manager registers through its `CursorRegistry`.
- `DesktopSession::load_icons` reads one asset per icon kind, from
  `/System/Graphics/Icons/<asset-id>.svg`, and returns an `IconSet` the
  taskbar installs through `TaskbarRenderer::set_icons`.

Both are **total and fail-closed per kind** (`AGENTS.md` §2.9): a kind whose
asset is missing, unreadable, malformed, or out of subset keeps its built-in
artwork, so a corrupt or absent `/System/Graphics` can never blank the
pointer or a status icon — it simply yields the built-in set.

The shipped **raster** icon masters in the same store
(`/System/Graphics/Icons/<asset-id>.png`) are not loaded as a set: they are
resolved one at a time, on demand, at the exact pixel side a slot draws at,
through the shell's artwork cache above. The path spelling is
`tairix_icon`'s (`GRAPHICS_DIR`, `icon_artwork_path`, `icon_vector_path`), so
this crate names the store's layout nowhere of its own.

## The desktop icon surface

`Desktop<S: DirectorySource>` (`src/desktop.rs`) is the user's own `Desktop`
folder shown as a column of icons down the screen's trailing edge, composited
by the window manager's own desktop layer (`Compositor::set_desktop`) —
beneath every window and reachable through no window id. It is a *directory
view*, not a new kind of surface: it lists the folder through the same
`DirectorySource` seam the trusted file picker uses, orders the listing with
the shared `sort_entries`, classifies each child with the shared content-type
registry, and lays its tiles out with the shared `GridView` under
`GridFlow::ColumnsFromTrailing` and `GridFill::FixedPitch` — the same cell
geometry and hit-test the file manager's row-major grid uses, just anchored to
the trailing edge, growing a new column inward as it fills, and keeping the
pitch rather than spreading a column's leftover space as the resizable file
manager does, so an icon does not drift when the work area's extent changes
(`lib/browse::layout`). It paints through the shared
`grid_tile`/`grid_metrics` and the shell's own icon-artwork lookup, so
a folder shows the shipped folder artwork and a file its content-class
artwork, falling back to built-in glyphs exactly as the file manager's grid
does.

**Pointer and keyboard.** A primary press selects the icon under it (or
clears the selection on empty desktop) and arms the shared
`DoubleClickTracker`, so a second press within its window activates the icon
— the desktop can never disagree with the file manager about what a gesture
means. Motion drives hover feedback and, on arrival from elsewhere, the
gesture-driven re-list below. While the desktop holds the keyboard, the
arrows move the selection (down/up one icon, left/right one whole column),
`Enter` activates it, and `Escape` clears it.

**Activation** resolves by entry kind: a directory opens the file manager
*at that path* (passed as the program's own first argument, which the file
manager now honours); an application bundle launches directly; a plain file
resolves its association through the catalog the session holds and launches
that application with the file as its argument; and a file nothing is
associated with is refused, stating the reason on the error stream rather
than failing silently. Every launch rides the session's existing
asynchronous launch path, so the compositor never blocks on one.

**Re-listing is gesture-driven, never timed.** There is no
filesystem-change notification in this system, so the desktop re-lists at
bring-up, after a session action that could have touched the folder, and on
pointer arrival from elsewhere — rate-limited by `RELIST_MIN_INTERVAL_NS` so
sweeping the pointer on and off the desktop cannot turn a gesture into a
re-listing loop. There is deliberately **no timer and no polling loop**: a
periodically-waking desktop would keep a core busy to discover nothing. A
re-list that actually changed the folder also refreshes the library catalog
and the file associations (`DesktopOutcome::relisted`), so an application
installed after bring-up is picked up without a restart.

## Fading the desktop in and out

The login screen fades to black and exits, so the screen a session inherits
is already dark. `ScreenFade` (`fade.rs`) fades the desktop up out of it
rather than snapping it on, driving the compositor's screen reveal from `0`
to `u8::MAX` over the active theme's own `SessionFade` duration — no timing
is spelled here. Logging out and stepping aside for another account run the
same fade the other way, so the desktop dissolves into the black the login
screen then appears out of, and a resumed session fades back in exactly as
a fresh one arrives.

It is one `tairix_theme::Fade` and nothing else, so both degenerate cases
need no branch: a reduced-motion theme answers a zero duration, which starts
settled and leaves the desktop simply revealed (or simply black) on its
first frame with no extra present and no timer; and a clock that jumped
backwards settles rather than stalling. A fade turned around picks up the
strength that is actually on screen, so a log-out chosen while the desktop
is still revealing dims from where it had got to rather than flashing
bright. The reveal begins at the session's first successful present, because
the span is wall-clock and one begun over bring-up would spend itself on an
empty screen.

The run loop's existing park drives the reveal: each wake steps the fade, and
the park is shortened to its next frame through `park_within` — the one fold
the lock screen uses too, so the two cannot round a deadline differently.
With nothing animating the park is byte-for-byte the value the loop already
carried, so an idle desktop arms no timer. Time alone finishes it, so a
display that refuses a present mid-fade cannot strand the desktop dark and
nothing retries.

The departure cannot ride that loop: it must run to completion before the
seat is given up, and the wake sources it is no longer serving would report
ready on every re-park and spin a core through the whole fade. `run.rs`'s
`fade_to_black` drives it instead, presenting each frame and sleeping
between them on `tairix_rt::park_ns` (the runtime's off-CPU timed park), and
it is bounded by the fade's own span. A refused present stops the dim where
it got to; the seat is handed on cleared regardless, so the screen still
ends black. Under a reduced-motion theme it is black from its first frame
and parks not at all.

`SeatPresentation` (`switchuser.rs`) orders the two into the switch: the
dim runs only after the authority *accepted* the step-aside — a refusal must
leave the screen exactly as the user left it — and before `suspend`, while
the seat and the frame ring are still up. Coming back, the arrival is begun
after `reconfigure` and before `repaint_all`, so the repaint presents the
first frame of the fade rather than the whole desktop at once.

Reaching full strength is announced once per session as `DESKTOP_REVEALED`
(message `DESKTOP_REVEALED_MESSAGE`), emitted after a present that reached
the display — the witness that the desktop is *visible*, not merely
presenting, since every frame before it is black to a degree. A
reduced-motion theme is settled on its first frame, so its witness lands
there; nothing is ever said twice. A fade heading for black never announces
at all: it reaches black, not visibility. The desktop QEMU verticals gate their
screendump on the rendered message, so it is defined once here and imported
by `tools/xtask` rather than restated.

Emitting it needs `CAP_LOG_EMIT`, which the bundle's manifest requests and
the interactive-account ceiling carries (`tairix_users::SESSION_BASELINE`),
so the intersection keeps it. The same grant is what finally lets the
session's cache ledgers reach the log: they were wired to it long before any
account could hold it, and were silently discarded until now.

Each served window gets the same treatment of its own: `WINDOW_SHOWN`
(message `WINDOW_SHOWN_MESSAGE`, naming the window in a `window` field) is
announced once per window, from the same place and for the same reason —
after a present that reached the display, because until that frame lands
nobody has seen it. Only the session can say this: an application learns its
present was *accepted* and the compositor that a frame was *composed*, and an
observer of the window channel cannot tell a present from a blur change, a
retitle or a declaration, since all four answer with the same four-byte
status reply. `SessionWindows` tracks each window as awaited, then painted by
its own first present, then shown, so a window whose body is still the
session's opening fill says nothing and a refused present leaves it awaited.
The icon-bar QEMU vertical gates both its screendumps and its bar gestures on
it.

The screen lock is the login screen's own surface, and the session hands it
the same real clock: `ScreenLock::handle` takes the instant, `advance` steps
it on every wake, and `park_deadline_ns` folds what the surface asks for
into the same park. A refused unlock therefore shakes as it does at login,
and an idle lock screen arms no timer either.

## Presenting the taskbar through the window manager

`TaskbarPresenter` joins the taskbar to the compositor. The taskbar paints a
*rectangular* `tairix_raster::Surface` and the window manager composites and
rounds windows; neither depends on the other (`AGENTS.md` §17.4), so the join
is session glue. Given a `&mut tairix_wm::Compositor` and the taskbar's own
`TaskbarRenderer` (which holds the across-frame glyph cache), `present`:

- paints the bar, places it at `BarLayout::bar`'s origin, and rounds it with
  `Corners::from_radius(BarLayout::corner_radius)` — the compositor's single
  anti-aliased rounded-corner path, the same one it uses for application
  windows, never a second one (`AGENTS.md` §2.2);
- while the program-library popup is open, paints its panel, places it above
  the bar at `LibraryLayout::panel`'s origin, and rounds it the same way;
  closing the popup removes the popup window.

Each of the five surfaces is placed asking for a backdrop blur of the theme's
`chrome_backdrop_blur`. They are drawn with the theme's floating ground — each
surface's own colour role at the palette's `chrome_alpha` — which reads as
frosted glass only over a blurred backdrop, so the two
are one decision and are taken from the theme at the one place a surface is
placed — never after, where a window would show for a frame wearing the
frosting of whatever was placed before it. The pinboard's own backdrop menu
goes through the same placement and asks for none: it covers what it opens
over. The bar itself now floats clear of the screen edges it faces
(`Metrics::taskbar_margin`), so all four of its corners round against
wallpaper; `DesktopShell::work_area` still keeps a maximized window out of the
whole band from that screen edge to the bar's inner side, gap included.

`present` repaints **only the surfaces the taskbar latched as changed**
(`TaskbarRepaint`, drained once by `DesktopShell::present`). The bar, the
library popup, the context menu, the notification popover, and the capsule's
instrument readout each cost a full re-render and a full window damage
rectangle, so a pointer crossing one small open menu repaints that menu and
leaves the other four exactly as they are. Two things override an empty
latch: a surface that has no window yet is always painted, so the first frame
puts everything on screen; and a change of desktop density repaints
everything, because the scale belongs to the output rather than to the
taskbar model the latch tracks.

The presenter owns only the two compositor `WindowId` tokens it minted, so the
session composes the GUI crates without holding the window-manager handle. It
is total and fails closed (`AGENTS.md` §2.9): a render that cannot allocate
leaves the on-screen window untouched, a window the compositor no longer knows
is re-created on the next present, and `teardown` removes both windows.

## Routing one input stream to both routers

The desktop has two input routers — the window manager's `InputRouter` and the
taskbar's `TaskbarInput` — and both consume the **same** shared `tairix_input`
event vocabulary (`AGENTS.md` §17.4, §2.2). A real input source produces one
stream, so `SessionInputRouter` fans it to the right router through
`handle(event, &mut Compositor, &mut Taskbar, now_ns)`. The monotonic `now_ns`
is threaded down because one taskbar gesture is decided by *time*: the
Switchboard capsule tells a tap from a hold by how long its press has been
down when the next event arrives, so the bar is handed the embedder's clock
reading rather than reading a clock of its own — the same instant every router
sees, and the one an in-memory test controls:

- while the **bar's context menu** OR **program-library popup** is open it is
  modal: every press, release, scroll, and key event routes to the taskbar;
  motion is still tracked by the window manager but its outcome is discarded;
- otherwise a **press** goes to the taskbar iff the pointer is over the bar (a
  secondary press there opens the menu that application declared; a middle press over the
  Switchboard capsule switches to the previous task) or over one of its open
  non-modal popovers, and to the window manager elsewhere — never both;
- a **scroll** over the Switchboard capsule or its open readout routes to the
  taskbar (it cycles the running tasks); every other scroll goes to the window
  manager;
- **pointer motion** is fanned to both so their pointers stay in step; the
  window manager acts on it (dragging a grabbed window) and the taskbar
  refreshes its launcher hover feedback. Motion is also where a capsule press
  held past the bar's long-press threshold resolves, and that is a real
  action: it takes the outcome while the drag still applied;
- a **primary release** goes to the taskbar *first* — a quick press on the
  Switchboard capsule resolves on its release — and one the bar does not claim
  ends an in-flight window move-grab in the window manager instead;
- a **key event** goes to the window manager — which delivers them to the focused
  window — except while a modal surface is open (above);
- anything else, including a non-primary release, is
  `SessionInputResponse::Ignored`.

The capsule's two gestures are the bar's own decision, not the session's: a
quick press asks for the running-task list and a press held past the
long-press threshold asks for the recovery list, each emitted as one
`TaskbarResponse::OpenSwitchboard { section }` the session only relays.

Decorations arm a title-bar drag through `begin_move`; the embedder reads the
keyboard owner through `focused`. The router holds no pixels and grants itself
no authority; every routed sub-call is total and fails closed (`AGENTS.md`
§2.9).

## Driving the desktop from a live input stream

`DesktopShell` composes all of the above — the `DesktopSession`, the
`SessionInputRouter`, the `TaskbarPresenter`, and the `TaskbarRenderer` — into
one event-driven frontend, the long-open "feed the router and presenter from
live device events" thread:

- `pump(source, &mut Compositor, now_ns)` drains the pending events from an
  injected `InputSource` seam (a real pointer/keyboard channel on a running
  system, an in-memory queue in tests, `AGENTS.md` §7), routing each through
  the `SessionInputRouter` and returning a `ShellOutcome` per event. One drain
  is one instant: the embedder reads the monotonic clock once when the source
  wakes it, and every event of that batch resolves the capsule's
  tap-versus-hold gesture against the same `now_ns`.
- A taskbar response is applied where the shell's own state suffices (a task
  activate/minimise outcome drives the compositor) and surfaced as
  `ShellOutcome::Taskbar` for the embedder; the bar is re-presented exactly
  once per event at one site, straight from the taskbar's drained per-surface
  repaint latch. Every model change that alters what a surface draws latches
  that surface, so an opened/closed popup or a hover reaches the screen
  without double-painting, and a motion that crosses no control — over the
  desktop, over a window, or over dead space on the bar — repaints nothing at
  all.
- `set_library` hands the popup the merged catalog (refreshing an open popup
  in place) and `raise_window` shows, raises, and focuses a tracked task's
  window — the Files button's idempotent open.
- A faulting `InputSource` ends the `pump` with its `Errno`; the events drained
  before the fault stay applied and the embedder replaces or re-polls the
  source (`AGENTS.md` §2.9 / §19.5).

The shell holds no framebuffer: the `Compositor` is the embedder's and is
passed in on each call. A loaded notification-icon set is installed with
`set_icons`, a title-bar drag armed with `begin_move`, and the desktop torn
down with `teardown`.

## Live device input source

`DeviceInputSource` (the `device` module) is the live backing for the shell's
`InputSource` seam. It wraps an injected `PointerInputChannel` — a
capability-checked kernel input channel on a running system, an in-memory queue
in tests (`AGENTS.md` §7) — that hands the desktop one framed
`tairix_abi::input::PointerInput` record at a time. Each `poll` decodes one
record through `PointerInput::from_bytes` into the `lib/input` `InputEvent` the
window manager and taskbar route: an absolute `PointerMoved`, or a
`PointerPressed` / `PointerReleased` carrying the resolved `PointerButton`. The
crate holds no input capability of its own — the channel delivers the bytes and
the decode runs above the device (`AGENTS.md` §17.4 / §19.5) — and a malformed
record fails closed with its `Errno` rather than being misinterpreted. The ABI
record is the desktop-level pointer event, a distinct layer from the
device-level driver input ABI, not a duplicate of it (`AGENTS.md` §2.2).

## Live keyboard input source

`KeyboardInputSource` (the `keyboard` module) is the keyboard counterpart of
`DeviceInputSource`. It wraps an injected `KeyInputChannel` — a
capability-checked kernel keyboard channel on a running system, an in-memory
queue in tests (`AGENTS.md` §7) — and each `poll` decodes one framed
`tairix_abi::input::KeyInput` record through `KeyInput::from_bytes` into the
same `lib/input` `InputEvent` stream the shell pumps: a `KeyPressed` /
`KeyReleased` carrying the resolved `Key` (a produced `Char`, or a `NamedKey` —
the twelve wire function-key codes fold into one `NamedKey::Function`) and the
held `Modifiers`. The `SessionInputRouter` routes it to the window manager,
which delivers it to the focused window. Like the pointer source it holds no
input capability and fails closed on a malformed record (`AGENTS.md` §5.4 /
§2.9).

## Seat-backed input channels

`SeatInputChannel` (the `seat` module) is the kernel backing for both the
`PointerInputChannel` and `KeyInputChannel` seams above: it drains each
fixed-width input record from the per-seat, owner-gated channel the kernel
seat registry routed the desktop's input to (`plans/DISPLAY.md`;
`docs/src/desktop/seat.md`). The records arrive through an injected
`SeatEventReader` seam — the seat-addressed `pointer_read` / `keyboard_read`
syscalls (`tairix_rt::pointer_read` / `tairix_rt::keyboard_read`) on a
running system, an in-memory queue in tests (`AGENTS.md` §7) — so the crate
holds no seat lease of its own and stays host-testable (`AGENTS.md` §17.4).

The security property is kernel-side: every drain is gated on
`CAP_INPUT_READ` **and** owner-gated against the seat's live lease, so only
the session that acquired the seat receives the stream. Desktop input is
deliberately not a named IPC port — a port's receive gate is capability-only
and cannot express "only the live seat-lease holder may drain". The
channel's own validation is narrow and fails closed (`AGENTS.md` §5.4 /
§2.9): an empty drain is `None`, and a drain of anything other than exactly
one whole record surfaces `LengthOutOfRange` rather than handing truncated
bytes to the decoder. A pointer record and a key record are each a
fixed-width drain, so the channel implements **both** seam traits through
one shared validation path rather than two (`AGENTS.md` §2.2); which records
flow is decided by the reader it wraps. Wrap a pointer reader in
`DeviceInputSource`, or a keyboard reader in `KeyboardInputSource`.

## What an application's presented frame costs

An application repaints its whole composition and presents whole-window
damage, because a toolkit generally cannot say which pixels its own paint
touched. `ShellWindowHost::window_presented` therefore converts the presented
pixels into the compositor's own content surface **and measures what actually
changed while it does so**: the conversion returns the bounding rectangle of
the pixels whose value differs, and only that rectangle is marked dirty. A
hover highlight a few rows tall costs a few rows of recomposition instead of
a whole window, and a repaint that changes nothing at all costs nothing. The
comparison is exact — a pixel reported unchanged carries the byte-identical
value it already had — and it rides a loop that already reads the frame and
writes the surface, so it adds one read per pixel and no allocation.

The conversion also validates every index it will use *before* the first
write, so a malformed or hostile geometry refuses the whole present and
leaves the window exactly as it was, never half-converted.

## Running-task list ↔ window stack

`TaskBridge` keeps the taskbar's running-task list in step with the window
manager's window stack. The taskbar models a list — one entry per top-level
window, with the click-to-activate / minimise rule — but owns no window
manager, and the window manager owns no task list (`AGENTS.md` §17.4). A task
is named by a `TaskId` and a window by an opaque `WindowId`, so the bridge owns
the correspondence: it mints a stable task id per tracked window and translates
between the two. Every operation is total and fails closed (`AGENTS.md` §2.9):

- `open` adds a window to the compositor, lists it as a running task, and
  shows, raises, and focuses it; it opens nothing only if the task-id space is
  exhausted.
- `close` removes the window and its task and drops focus if it held it; an
  untracked window is a no-op.
- `raise` shows, raises, and focuses a window — what a chosen picker cell and
  a raise-the-application click both ask for — and is a no-op for an unknown
  one.
- `minimize` is the title bar's control's counterpart: it marks the entry
  minimised, hides the window, and drops focus if it held it.
- `sync_focus` mirrors a window-manager focus change back into the bar's
  highlight, leaving it untouched (and forcing no repaint) for a focused window
  that owns no task.

`DesktopShell` drives it: `open_window` / `close_window` manage the lifecycle,
and `handle` applies a `WindowChosen` outcome to the compositor and mirrors a
window-manager focus change into the bar, moving keyboard focus through the
window manager's `InputRouter::focus` / `unfocus`. The bridge holds no pixels
and grants itself no authority — the compositor, router, and taskbar are the
embedder's, passed in per call.

## The Switchboard channel and hang detection

The taskbar's right-most Switchboard capsule renders live state from two
independent, honest feeds, and the session talks back to the monitor service
over its own mailbox (`plans/NEW-TASKBAR.md` T9–T11).

**The published summary.** The `Run` binary binds the seat-scoped
`SWITCHBOARD_ENDPOINT` beside the window and notification rendezvous. Every
request on it is attested first: the caller's kernel-provided
`call_peer_origin` pid must match the launch table's live entry for
`SWITCHBOARD_RUN_PATH` — a foreign process, an orphan of an earlier session,
or a hand-launched copy is a typed refusal stated on `stderr`, never rendered
(`AGENTS.md` §5.4). An accepted `PublishSummary` reaches the capsule through
`DesktopShell::set_tray_summary`; when the service exits, the reap path clears
the feed so the capsule falls back to calm rather than freezing a dead
service's last summary. The reply to a successful publish is
`encode_publish_reply` carrying **this session's own `ProcId`** — the identity
the kernel attests to the process itself (`tairix_rt::self_origin`), read once
at bring-up and reused, never re-derived a second way, and the very reading
the window server is constructed with. That reply is how the service learns
the one identity whose commands it will accept, so the reverse direction is
authenticated too. A **refusal** stays the plain status frame: an unattested
caller learns nothing about the session it failed to reach.

**The two owner-directed requests.** The monitor's panel acts on *other*
processes' windows, and the session is the only component that may. Both are
attested exactly like a publish and then validated against what this session
can actually see:

- `ActivateOwner { owner }` is authorised against the **live window
  registry** — the owner must hold a served window on this seat *now*,
  resolved through the window engine's attested ownership records — and
  raises that owner's front window through the session's one focus/raise path.
- `RestartOwner { owner }` is authorised against the **launch table** — the
  owner must be a child this session itself spawned, so its bundle is the
  attested one it was launched from — and re-launches it through the session's
  one attested spawn-and-record path.

An owner this session cannot act on is `Errno::NotFound`, stated on `stderr`
and never guessed at. No refusal mutates the model, and neither request adds a
second raise or launch route (`AGENTS.md` §2.2). The serving decision itself is
pure and host-testable: `serve_switchboard_request` takes a borrowed
`SwitchboardServe` — the shell, the compositor, the launch table, the
`OwnerWindow` seam, the relaunch closure, and the session's `ProcId` — and
returns a `SwitchboardOutcome` or a `SwitchboardRefusal`.

**The command mailbox the session sends on.** The session sends to the
instance's own mailbox, `command_endpoint_for(<the service pid the launch
table holds>)`, as a **non-blocking** send: the desktop loop never blocks or
spins on a panel that is slow to drain, so a send refused for a full or absent
mailbox is reported on `stderr` and dropped, never retried (`AGENTS.md`
§2.23). Two commands travel it:

- `OpenPanel { section }` — the capsule gesture. The bar decides the section
  (a quick press its running-task list, a hold its recovery list) and emits
  `TaskbarResponse::OpenSwitchboard { section }`; the session only maps that
  choice onto the wire vocabulary. With **no instance live** the press is
  itself the demand for one: the session revives the service through the same
  bring-up path and holds the section as *one* pending open — replaced, never
  queued — delivered on that instance's first publish (the proof it is up and
  listening) and cleared, so it is never re-sent on a later publish.
- `SeatReport { report }` — the unresponsive-owner view of this seat, from the
  session's own delivery evidence. Every app-ward window event is a
  non-blocking mailbox send. To avoid flooding an app with a dense gesture
  it must drain one sample at a time from a bounded mailbox, `pump` folds
  an adjacent run of one gesture over one window: motion to the latest
  position, and wheel ticks in one direction to their sum (a reversal ends
  the run). Every sample still
  drives the window manager's own state. The production event sink folds
  each outcome into the `vigil::HangTracker`: an owner whose sends come
  back refused as the kernel's transient `WouldBlock` backpressure signal
  continuously for `UNRESPONSIVE_AFTER_NS` is flagged *not responding*, one
  accepted delivery clears it, and a reap forgets it. No heartbeat is
  fabricated and no kernel query pretends to know. The refused event itself
  is **held**, not dropped — see the hold-back below.
  The report is sent **only when the tracked set actually changes** (the
  tracker's change latch, drained once per wake), never per frame and never
  polled, and it carries the truthful `total` even when more owners are hung
  than one frame can name — the id list is bounded by
  `SEAT_REPORT_OWNERS_MAX`, so the monitor sees an honest count alongside the
  ids it can act on rather than a silently truncated one.

## The app-ward hold-back (`holdback`)

An app's event mailbox is bounded, so a merely slow app fills it and a send
comes back `WouldBlock`. That means the app is behind, not gone, so the
event is **owed** rather than dropped: dropping a `Resized` leaves the app
laying out at a size the compositor no longer uses, and dropping a
`FilePicked`/`PickCancelled` strands that window's one pending pick for the
rest of its life (the engine clears the pick only once the conclusion is
*accepted*).

- **Parked, never polled.** The first debt to a destination arms a
  `WaitSourceKind::PortRoom` member on the app's own event port — the
  send-side twin of the `Port` member. The app's drain frees a slot, the
  kernel wakes the session, and it sends what it owes; the member goes once
  the destination owes nothing. No capacity poll, and the desktop still
  never blocks on an app (`AGENTS.md` §2.23).
- **Ordered.** A destination already owed something takes the next event
  unsent, so nothing overtakes what is queued. Queues are per
  `(destination, window)` and a flush serves an owner's windows
  round-robin, so one window's backlog starves no sibling.
- **Folded by meaning.** A state edge (`Focus`, `Resized`,
  `RedrawRequested`, `CloseRequested`, `Minimized`, `DesktopChanged`) is a
  value the app converges on, so a later one replaces the held one in
  place. A position is latest-wins and a wheel run sums — the same rule
  `pump` applies live, from one shared predicate. Keys, buttons, and the
  pick conclusion are owed in full.
- **Bounded.** `HOLD_BACK_CAPACITY` (64 per window) caps what an app that
  never drains can make the desktop hold: a security bound, not a scalable
  capacity (`AGENTS.md` §24.4). Overflow sheds the oldest *input* event —
  total, because folding leaves at most six state edges and one conclusion,
  and safe, because a press is shed before its release.

## The `Run` binary — the live desktop session (`plans/DISPLAY.md` D7c)

The crate also ships the desktop session's `Run` entry-point binary
(`src/run.rs`, built freestanding on the native Tier-1 targets and an inert
host stub elsewhere), the first live embedder of everything above. It wires
the real seams end to end:

- `display_acquire(SEAT_PRIMARY)` binds the session as the boot seat's
  owner; the kernel owner-gates every later drain and present against that
  live, revocable lease — the session asserts nothing itself.
- `DisplayClient` over `ipc_call` to the reserved `DISPLAY_ENDPOINT`
  performs the bring-up handshake: query the mode (checked frame
  arithmetic, fail closed on overflow or a zero-sized mode), `shm_create`
  the double-buffered frame region, `shm_grant` it **to the serving task of
  the display endpoint** (never a raw, recyclable PID), configure, then
  present by frame index through `RemoteDisplay` — no frame bytes ever
  cross the IPC.
- The `DesktopShell` is driven from the two live seat readers (the
  seat-addressed `pointer_read` / `keyboard_read` behind the
  `SeatEventReader` seam), with the queried mode as the pointer's screen
  rectangle and the compositor's background taken from the active theme's
  desktop colour.
- The session **parks on a `SeatInput` wait-set member** between events —
  never a poll loop — woken by input delivery *and* by lease loss. Losing
  the seat (the typed `SeatRevoked` / `SeatNotOwner` on any drain or
  present) tears the session down fail-loud.
- It spawns the Switchboard monitor service
  (`/System/Services/switchboard.app`) as the logged-in user, serves its
  `SWITCHBOARD_ENDPOINT` requests through `serve_switchboard_request` behind
  the `call_peer_origin` attestation, and sends `OpenPanel` / `SeatReport` on
  the instance's `command_endpoint_for` mailbox — the same one bring-up path
  revives a dead service on demand, and a refused send is reported and
  dropped rather than retried.
- App-ward window events go through `RtEventSink`, which owns the hold-back
  above: a refused event is held and its destination watched for room, and
  the loop's room-wake arm sends what is owed and tears down any owner a
  send proves gone.
- **Fast user switching** (`switchuser`, `plans/NEW-DESKTOP-LOGIN.md` G5).
  Before the first frame the session binds its wake mailbox
  (`session_wake_endpoint(own_pid)`) and joins it to the wait-set for the
  process's whole life; a refused bind is stated and non-fatal — the desktop
  simply runs as one that cannot be switched away from, and the *Switch
  User…* row is left out. Stepping aside asks the authority **first**
  (`SessionRequest::Background` on `SESSION_ENDPOINT`) and gives the screen
  up only on `Accepted`: a refusal, an undecodable reply, and a transport
  failure are one answer — stay exactly as we are, and say so. On acceptance
  the frame ring is dropped, its region unmapped, the seat's wait-set member
  withdrawn (so the next account's typing cannot wake a parked desktop into
  a spin) and the lease released, in that order; the rest of the drained
  batch is discarded and the kernel purges what is still queued. While
  background the session presents nothing, drains no seat input, and parks
  with **no deadline at all** (`SwitchUser::park_deadline_ns`) — apps keep
  running and their window calls keep being served. A `Foreground` wake
  re-acquires the seat, re-queries the mode (it may have changed), rebuilds
  the frame region for it, has the compositor adopt it
  (`Compositor::set_mode`), re-lays the bar, wallpaper, icons, and pointer
  rectangle, and repaints the whole screen; any step refusing ends the
  session with its reason. A `SessionWake::End` ends it cleanly. Every wake
  is honoured only from a kernel-attested sender on this session's own
  console holding `CAP_IPC_BIND_PRIVILEGED` — the authority's account is a
  configured one, so the check is the capability and the console, never a
  guessed uid — and anything else is dropped with its reason stated.
- The binary branches into the **worker-role** at the very start of `main`:
  if re-entered with the reserved role argument it serves as the parser-sandbox
  icon-rasterisation service and nothing else, using its own image as the
  untrusted-decode host.

The manifest (`AppInfo.toml`) requests exactly `CAP_DISPLAY`,
`CAP_INPUT_READ`, and `CAP_SHM`. The bundle's image planting and the
end-to-end QEMU vertical ride the D7d autoload world (`plans/DISPLAY.md`).

## Dependencies and layering

The crate composes the other GUI crates and `lib/*` only — `tairix-taskbar`,
`tairix-wm`, and the shared `tairix-theme` definition, plus `tairix-cursor` /
`tairix-icon` (the SVG set builders) and `tairix-abi` (the `Errno` the read
seam returns and the `PointerInput` / `KeyInput` records the device and
keyboard sources decode)
(`AGENTS.md` §17.4). Composing GUI crates is the permitted
`userland/gui/*` edge; nothing outside `userland/gui/*` depends on it (§17.3),
so a headless image omits it cleanly.

The `Run` binary additionally links `tairix-display` (the client half of the
present protocol) and `tairix-rt` (the pure-Rust userland runtime), for the
bare-metal targets only.

The library is `no_std` with `#![forbid(unsafe_code)]`; no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). The `Run`
binary holds the one justified `unsafe` — the slice view of its own
kernel-mapped frame region, with its invariants stated in a `// SAFETY:`
block (`AGENTS.md` §2.10).

## Still to come (Stage 7, `plans/NEW-TASKBAR.md`)

The interactive light/dark switch in the Switchboard's System menu (T13),
relaying the active theme to apps over live IPC, and the VFS-backed reads of
the **SVG cursor and notification-icon sets** in the `Run` binary (the
in-memory-tested loaders and their fallbacks exist; the `Run` binary installs
the built-in sets until then). The raster **icon artwork** is already live
there: the `Run` binary binds the shell's artwork seams to the VFS reader and
the sandbox worker at bring-up.
