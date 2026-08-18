# Default desktop apps

The default graphical applications live under `userland/apps/`. They are
ordinary `.app` bundles (`AGENTS.md` §16.5) that consume the shared desktop
`lib/*` crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`,
`tairix-font` — exactly as the taskbar does, and never depend on the window
manager (`AGENTS.md` §17.4).

## Every app asks the desktop first

An application cannot draw honestly without knowing three things the session
owns: how large the screen is, what UI density the desktop runs at, and
whether the theme is light or dark. Guessing any of them is a defect — a
window that opens larger than the display, a layout at the wrong density, or
a window still dark after the user switched the desktop to light.

So every graphical app starts the same way, before it sizes or paints
anything: `WindowClient::desktop` returns the seat's
`tairix_abi::desktop::DesktopInfo`, and `tairix_window::Desktop` holds it.
From that, uniformly:

- `Desktop::window_size(logical_w, logical_h)` gives the size to open at —
  the app's preferred size, authored in logical pixels as every desktop
  length is, resolved at the live density and capped to the screen. There is
  one such call per app; none computes it itself (`AGENTS.md` §2.2).
- `Desktop::scale()` replaces what used to be a hard-coded 100%, and feeds
  every `BitmapFont::for_role` and every control's layout.
- `Desktop::appearance()` is applied to the app's `ThemeRegistry` *before*
  the first frame, so a window opened into a light desktop opens light.
- `Desktop::apply(&event)` is fed **every** delivered event. A
  `WindowEvent::DesktopChanged` — pushed to every live window when the
  session changes any of it — is adopted, and only a real change costs a
  re-theme, a re-layout and a repaint; an announcement that changed nothing
  costs none.

Both failure paths state their reason and neither invents a value: a refused
query, or a density this client cannot draw at, exits fail-loud with the
app's no-window code (`AGENTS.md` §2.24); a refused *change* is reported on
`stderr` and the app carries on at the last desktop it accepted, rather than
drawing at something it could not validate (`AGENTS.md` §5.4).

The query is read-only and carries no capability: it describes the caller's
own seat, names no other principal's data, and grants no authority. See
[Variable DPI and UI scale](./dpi.md) and
[Desktop session glue](./session.md).

## An overlay is a popup surface, never pixels in the app's own window

A menu, a settings sheet, a tooltip, or any other transient overlay drawn
*inside* an app's window surface is clipped the moment the user shrinks that
window, and cannot be larger than its owner. So an app opens each overlay as
its own **popup surface** — undecorated, positioned by the app, stacked
directly above the window that owns it (`plans/APPWIN.md` AW6; the protocol
side is [Window manager](./wm.md#app-owned-popup-surfaces)).

Uniformly, for every app:

- `WindowClient::create_popup(&PopupSpec { .. })` names the app's own parent
  window, the popup's granted frame region and event endpoint, its geometry,
  and an offset in physical pixels **from the parent's client origin** — an
  app is never told its own window's screen position, so it never computes a
  screen point. The session resolves the parent's live origin and clamps the
  whole popup onto the screen, which is why an overlay *larger* than its
  owner's window (a settings sheet over a tiny window) opens whole: the app
  asks for the offset that would centre it and lets the clamp do the rest.
- The popup is sized from the **overlay's** own preferred extent, measured
  against the screen rather than the window — so shrinking the window can no
  longer shrink the menu or the sheet.
- The overlay's events arrive under the popup's own window id with
  **popup-local** coordinates, so one event mailbox serves both windows and
  the app demultiplexes on `WindowEvent::window_id`, hit-testing the overlay
  against the popup's own viewport. The app's own window keeps drawing
  nothing of the overlay.
- Dismissing the overlay closes the popup and unmaps its region; closing the
  app's window takes any popup with it, and a popup counts against the same
  per-client window budget as an ordinary window.
- Showing an overlay is an incidental, refusable action: a refused popup is
  stated on `stderr` and simply not shown — never a crash, and never a
  fallback to a clipped in-window draw (`AGENTS.md` §2.24, §5.4).

The graphical terminal is the first consumer: its right-click menu and its
settings sheet are each a popup (`plans/GUI-TERMINAL.md` §9).

## Filesystem browser (`tairix-files` over `lib/browse`)

The filesystem browser navigates the §16 filesystem layout and renders the
current directory through the active theme. It is split into a navigation
**model** and a **renderer**, both driven by an injected directory-read seam,
so the security-relevant logic is testable without a kernel (`AGENTS.md` §7).
The engine — the model, the renderer and its row hit-test, and the validated
path spelling described below — lives in the shared `lib/browse` crate
(`tairix-browse`), because the desktop session's trusted file picker
(`plans/APPWIN.md` AW5) drives exactly the same engine; the `tairix-files`
package is only the `Run` binary that composes it over the live syscalls.

### The directory-read seam

`DirectorySource::list(components)` returns the children of an absolute path
(root-first components; the empty slice is `/`). On a running system the seam
is a capability-checked VFS directory read, so the §5.3 permission decision and
the §16 path policy live in the VFS, not in the app. The browser shows exactly
the entries the source returns — it never fabricates a `/proc`/`/sys`-style
synthetic entry (`AGENTS.md` §16.1). Each entry is an `Entry` carrying a name,
an `EntryKind`, and the display metadata a file manager needs (see below).

### Entries, kinds, and the shared sort

An `Entry` carries its name, its `EntryKind`, its apparent `size`, and its
last-modification `Time64` — the size and timestamp mapped straight from the
one `fs_readdir` stream the source already produced (each
`tairix_abi::fs::DirEntry` reports them), so the browser never opens and
`fs_stat`s every child to fill a listing (`AGENTS.md` §2.16). `EntryKind`
refines the VFS's file/directory split with the one distinction a manager
must make structurally: a `<Name>.app` directory is a `Bundle` — a sealed
unit the user launches, not a folder to descend into (`AGENTS.md` §16.5). The
engine only *models* the distinction (`Entry::is_bundle`, and `is_directory`
is `false` for a bundle so `open_index` refuses to descend); deciding what a
bundle activation *does* is the launching layer's job (staged for a later
increment).

`SortMode` (`SortKey` — `Name`/`Size`/`Modified` — plus a `SortDirection`)
is the one listing order both the file manager and the trusted picker share
(`AGENTS.md` §2.2): directories first, then the chosen key, with a
case-insensitive name tiebreak so the result never depends on the source's
incidental order. `sort_entries` is the pure definition; the `Browser`
applies it to every listing and `set_sort_mode` re-orders in place, keeping
the selection on the same entry. The default is name-ascending — a
general-purpose directory order.

### The production source (`vfs`)

`VfsDirectorySource` is the shipping `DirectorySource` (`plans/APPWIN.md`
AW1). It composes three pieces, each host-proven:

- `spell_absolute_path` — the app's one path spelling, shared by the
  browser's displayed path, the tests' tree keys, and the VFS fetch, so the
  three can never disagree (`AGENTS.md` §2.2).
- `absolute_path` — validation before spelling: a component that is empty,
  `.`, `..`, or carries `/`/NUL is refused (`OutOfRange`) *before* any
  syscall, and the spelled path is bounded by the kernel's `FS_PATH_MAX`
  (`LengthOutOfRange`) — validate every input, fail closed (`AGENTS.md`
  §5.4).
- `entries_from_dir_stream` — the packed `fs_readdir` stream mapped onto
  `Entry` values through the shared `tairix_abi::fs::DirEntries` walker (the
  same walker `ls` lists through); one malformed record or non-UTF-8 name
  refuses the whole listing, never a partial one.

The directory fetch itself is injected (`fetch(path) -> stream`): the
shipping program passes `tairix_rt::read_dir_all` — the kernel-authorised
`fs_open` + grow-to-`FS_IO_MAX` `fs_readdir` transfer under the app's own
attested identity — while tests pass an in-memory tree of encoded streams and
drive a `Browser` over it end to end. The engine adds no authority and makes
no permission decision of its own.

### The navigation model

`Browser` holds the current directory's path and entries plus a selection
cursor:

- `open_root` opens at `/` and lists it; `open_at(source, components)` opens
  *at* a given directory (root-first `components`, empty being `/`, so
  `open_root` is exactly `open_at(source, [])`). The window title carries that
  path and `go_up` climbs from there, with a fresh (empty) history — the one
  way a consumer starts somewhere other than `/` without a second navigation
  model. The desktop session's trusted picker opens at the user's home this
  way (below).
- `open_index` / `open_selected` descend into a directory entry; `go_up`
  climbs to the parent and returns `Ok(false)` at the root (no parent is not
  an error).
- `refresh` re-reads the current directory, clamping the selection into the
  new listing.
- `select`, `select_next`, and `select_previous` move the selection, clamping
  at both ends.

`Browser` also keeps a bounded **navigation history**, which the toolbar's
Back / Forward tools drive:

- `go_back` / `go_forward` walk a back / forward stack; `can_go_back` /
  `can_go_forward` report whether each move is available, which is exactly
  the enable state of the Back / Forward toolbar controls. Any fresh
  navigation (descend, climb, or a jump to a location) records the directory
  it left on the back stack and clears the forward branch, as a web
  browser's forward history is discarded on a new turn.

The history is a bounded ring: once it reaches its cap it drops the
*oldest* location rather than growing without bound. It is a UX
convenience, not a hardware-scaled resource, so the bound is a deliberate
defensive cap (§24), and reaching it never fails a navigation — it simply
forgets the least-recent step.

Every directory-listing move is **transactional and fails closed**
(`AGENTS.md` §5.4): the target is listed *before* any state changes, so a
refused or failing read leaves the browser on the directory it was already
showing — a `go_back` / `go_forward` / location jump to a directory that
has become unreadable leaves the browser *and its history* exactly as they
were. The fail-closed outcomes are the `BrowseError` variants: `Source`
(the wrapped boundary `Errno`, e.g. `PermissionDenied`), `NoSuchEntry`, and
`NotADirectory`.

### The frame model — the toolbar

The drawn window chrome — the `lib/controls` toolbar
(`plans/NEW-FILEMANAGER.md` FM4b) — is painted from a pure
`chrome` model, host-proven ahead of the widgets it drives, exactly as the
`Activation` and `open_with` decisions are:

- `ToolbarModel::for_browser(browser)` snapshots which `ToolbarCommand` is
  currently actionable. **Back / Forward / Up** reflect the navigation
  history and depth (`can_go_back` / `can_go_forward` / `!is_root`);
  **Refresh**, the **view toggle**, and **Sort** are always available.
  `is_enabled(command)` gives the drawn button its enabled state — an
  unavailable tool renders *disabled*, never hidden, so the toolbar's shape
  stays stable — and `view_mode()` / `sort_mode()` give the view toggle and
  sort control their current (pressed) state. `TOOLBAR_COMMANDS` is the one
  left-to-right command order the chrome iterates.
- `ContextMenuModel::for_browser(browser, has_clipboard)` snapshots which
  `ContextCommand` the right-click menu offers is actionable. **Open**,
  **Rename**, **Cut**, **Copy**, **Properties**, and **Delete** act on the
  selected entry, so they need a selection (an empty directory offers none).
  **Open With…** is offered only for a regular file — a directory descends and
  a bundle launches itself, so neither has an application to choose.
  **Pin to taskbar** is offered only for a bundle — pinning names an installed
  application, which no plain file or directory is (the model reads the
  selection through the one shared `EntryKind` classifier).
  **Paste** targets the current directory and needs only a held clipboard,
  not a selection; because the clipboard lives in the app rather than the
  browser (`Browser::clipboard` *captures* a fresh one from the selection),
  whether a paste is possible is the app's own state, threaded in as
  `has_clipboard`. `is_enabled(command)` gives each drawn `MenuItem` its
  enabled state (an inapplicable command renders *disabled*, never hidden),
  and `CONTEXT_COMMANDS` is the one top-to-bottom order the drawn menu
  iterates.

The model decides *what is offered*; it performs
no navigation or I/O itself, so composing it grants nothing (the read-only
picker builds the same model). Only commands the file manager can actually
carry out today are modelled, so none is speculative surface (`AGENTS.md`
§2.4): **Open With…** joined the set with its FM6b chooser verb (below),
**Delete** joined it with FM9-c's confirm-and-remove verb (its `begin_delete`
action, below), and **Pin to taskbar** joined it with the taskbar-pin stage's
window-channel verb (`plans/NEW-TASKBAR.md` T7, below). **New Folder** is a
*write* tool that lives on the manager-only toolbar (below), not on this menu
shared with the read-only picker.

The **context menu is now drawn and clickable**. A secondary-button
(right-click) press selects the item under the pointer — or clears the
selection on empty space, so only the directory-scoped Paste is offered —
and opens a `lib/controls` `Menu` painted from the `ContextMenuModel`:
`render::build_context_menu` builds one `MenuItem` per `CONTEXT_COMMANDS`
entry (its `ContextCommand::label()` and keyboard-`shortcut()` caption,
rendered disabled when the model reports it inapplicable),
`render::context_menu_rect` anchors it at the click and clamps it inside the
window (delegating to the shared `lib/controls` `Menu::anchored_rect`, so a
second context-menu owner elsewhere in the desktop places its popup the same
way), `render::draw_context_menu` paints it topmost, and
`render::context_menu_command_at` mirrors that placement to return **only an
enabled command** (a press on a disabled row or off the menu resolves to
nothing, failing closed). The `files.app` `Run` binary routes a chosen
command through `dispatch_context_command` to the **exact same** app verbs
the toolbar and keyboard already drive — Open (`activate`), Open With… (the
chooser below), Pin to taskbar (the window channel's pin request, below),
Rename, Cut, Copy, Paste, Properties, and Delete (the same
modal-confirmed `begin_delete` the `Delete` key opens, below) — so the menu can
never diverge from them (`AGENTS.md` §2.2) and adds no authority (every verb is
the user's own §5.3-checked action). `Escape` or a press off the menu dismisses
it. Because the `Delete` key is not the only way in, the confirm-and-remove
flow is reachable by pointer alone (`render::context_menu_command_rect` is the
shared forward mirror of the hit-test, so a caller — including the desktop
integration harness — can aim at exactly the drawn Delete row, §2.2).

**Pinning an application to the taskbar** (`plans/NEW-TASKBAR.md` T7) has two
pointer spellings, both incidental to browsing and both resolved by the
desktop session under its own authority. Choosing **Pin to taskbar** from the
right-click menu on a bundle runs `pin_selected_bundle`: it re-checks the
selection is a bundle (fail closed), names it through the same validated
`selected_target_path` spelling every open/stat uses, and sends the window
channel's `WindowClient::pin_bundle`; a refusal — already pinned, a full bar,
or a session that does not pin — is stated in one terse `stderr` line and the
app simply carries on (`AGENTS.md` §2.24). Dragging a bundle out of the
browser is the other spelling: a primary press on a bundle row arms the pure
`drag::BundleDrag` detector (a press on anything else arms nothing), and the
first motion beyond its `DRAG_THRESHOLD_PX` (6 physical pixels, Euclidean)
sends **one** `WindowClient::drag_offer` per gesture with the same validated
path. What a drop means is the session's decision: a primary release ends the
gesture locally whatever mode the window is in, `Escape` sends
`WindowClient::drag_withdraw` exactly when an offer is outstanding, and a
refused offer — or a listing that changed under the held press — dies
silently, so the gesture never disturbs browsing.

The **toolbar is now drawn and clickable**. `render` paints the
`TOOLBAR_COMMANDS` as a `lib/controls` `Toolbar` of themed `IconButton`s in
the top strip, each glyph from `ToolbarCommand::icon()` and each rendered
enabled or disabled from the `ToolbarModel` (a disabled tool reads muted, not
hidden). A primary-button press resolves through `render::toolbar_command_at` —
which mirrors the drawn toolbar's own layout across the whole window and returns
**only an enabled command** (a click on a disabled tool or a group gutter
resolves to nothing, failing closed) — and runs through the one shared
`apply_command(browser, command)`. `apply_command` is a **read-only** dispatch
(history / climb / refresh / view toggle / sort cycle), so the picker drives the
same toolbar; Back/Forward/Up/Refresh are the browser's transactional, fail-closed
navigation, and the view toggle and sort each step to the next mode
(`ViewMode::toggled`, `SortMode::next` — a fixed six-mode cycle). The
keyboard drives the same dispatch through accelerators: **Alt+←/→** (Back /
Forward), **Alt+↑** (Up), and **F5** (Refresh), so a shortcut and a toolbar
click can never diverge (`AGENTS.md` §2.2). The view toggle and sort are
toolbar (pointer) commands; a conventional single-key accelerator for them
awaits the later toolbar keyboard-focus pass.

**The manager-only write tools.** The read-only picker composes the
exact same toolbar (`render`, `apply_command`), so a *write* action can never
live in the shared `ToolbarCommand` / `apply_command` surface — that would hand
the picker write authority. The manager tools are therefore a distinct
`chrome::ManagerTool` vocabulary (`MANAGER_TOOLS`, `ManagerTool::icon()`) —
New Folder, the **Go to Trash** location, and **Empty Trash**
(`plans/NEW-FILEMANAGER.md` FM11b) — that
**only a write-capable consumer hands to `render`**: the file manager passes
`MANAGER_TOOLS` (with a `chrome::ManagerToolModel` enable snapshot), the picker
passes an empty slice and `ManagerToolModel::none()`, so the picker cannot draw
or resolve a write tool (the separation is by type, not a runtime flag).
`render` draws the write tools in their own toolbar group after the read-only
commands — each muted (never hidden) when the model reports it inactive, so
Empty Trash reads disabled outside a non-empty Trash — and
`render::manager_tool_at` is their mirror hit-test, resolving **only an enabled
tool** (a read-only command's position is unchanged whether or not write tools
follow). The `files.app` `Run`
binary routes a click on the tool — and the **Ctrl+Shift+N** keyboard
equivalent — to a new folder: `mkdir::suggest_new_dir_name` names a
non-clashing placeholder, `Browser::create_directory` creates it through the
`fs_mkdir` seam under the user's own identity (**no new capability** — the
per-inode owner/mode/ACL model gates it), and the inline rename opens on the
new folder so the user names it at once. A refused create states its reason on
`stderr` and leaves the listing put — an answer, not a crash (`AGENTS.md`
§2.24, §5.4).

### Activating an entry

Opening an entry — a double-click, or `Enter` on the selection — is one
dispatch-by-kind decision, `Browser::activate_selected` / `activate_index`,
returning an `Activation` (`plans/NEW-FILEMANAGER.md` FM6). It lives in the
engine, not the app, so the file manager and the trusted picker act
identically (`AGENTS.md` §2.2). It is exhaustive over the three entry kinds:

- a **directory** is *descended into* by the engine itself (its own
  transactional, fail-closed navigation) and returns `Descended` — there is
  nothing for the caller to launch;
- an **application bundle** (`<Name>.app`) returns `LaunchBundle { path }`,
  naming the bundle for the caller to launch through the ordinary signed
  app-load gate;
- a **regular file** returns `OpenFile { path }`, naming the file for the
  caller to open in the associated viewer.

The target's absolute path is spelled through the one shared `absolute_path`,
so a launch or open can never name a different node than the browser shows;
a name that cannot be spelled as a valid, bounded absolute path fails closed
as `BrowseError::Source`, exactly as descending into it already does. The
engine holds **no** launch or open authority of its own: it decides *what* the
target is and *what should happen*, never performing the spawn or the
`fs_open` — those stay in the app's own capability-checked tail under the
launching user's identity (so the read-only picker composes the same
`Browser` and simply never launches).

The `files.app` `Run` binary acts on this decision when the user presses
`Enter`, **double-clicks an item**, and chooses **Open** from the right-click
menu — all three route through the one shared `activate`, so a pointer
double-click can never open something a keyboard `Enter` would not
(`AGENTS.md` §2.2). The double-click is the shared, pure
`click::DoubleClickTracker` (`plans/NEW-FILEMANAGER.md` FM12): a primary press
selects the item under the pointer, and a second press on that *same* item
within `DOUBLE_CLICK_INTERVAL_NS` (half a second, timed by the capability-free
monotonic `clock_get`) activates it. The tracker is reset whenever a press
lands on chrome (a toolbar tool) rather than an item, so a
click *through* the chrome and back is never mistaken for a double-click; a
non-monotonic clock reading fails closed to a single click. On any activation,
a `Descended` reveals the selection and repaints, and a
`LaunchBundle { path }` **launches the bundle** — its own `Launcher` spawns the
bundle's own `Run` (`<path>/Run`) through the ordinary signed app-load gate
(`CAP_PROC_SPAWN`, added to the manifest in the stage that first uses it),
under the launching user's identity and with no ambient authority. The launch
is **asynchronous and non-blocking** (`plans/FIX-DESKTOP.md`): `spawn` admits
the child and returns its PID before the image loads, so the event loop never
freezes behind a load; a synchronous refusal is stated fail-loud on `stderr` at
once, and a load refusal that only shows once the image is read surfaces later
as the child's reserved `LOAD_*` exit status, named by the reap (the shared
`load_failure_reason` wording). The manager **reaps** every launched child on a
new any-child wait-set member, drained in the event source's park branch the
instant it fires, so a launched app is never left a zombie and the wake never
degrades into a busy-poll (`AGENTS.md` §2.23). An `OpenFile { path }`
decision **opens the file in its associated viewer** — the inherited-document
hand-off, the TAIRiX spelling of `viewer < file`: the manager resolves the
associated application from the installed bundles' declared file-type
associations (`RtBundleSource` + `applications_for`, keyed off the file's leaf
name — never a hard-coded viewer path), opens the file **read-only in its own
table**, and spawns that bundle's `Run` with the descriptor wired onto the
child's `STDIN` slot (`FdWire::Handle`) plus the reserved `DOCUMENT_ROLE_ARG`
token and the leaf name for the window title. The kernel clones the read-only
open description into the child owner-checked, so the viewer reads its document
with **no filesystem capability of its own** (least privilege) and there is no
post-spawn channel or ordering race; the manager closes its own descriptor
immediately and reaps the child on the same any-child member as a launched
bundle. Launching is asynchronous and fail-loud: a file no installed
application claims leaves the listing unchanged and states the refusal on
`stderr`, never a fabricated open (`AGENTS.md` §2.24). The viewer detects
`DOCUMENT_ROLE_ARG` at start-up and displays the handed-over document instead
of prompting the session's trusted picker (its standalone launch is
unchanged).

The explicit **"Open With…"** chooser is now drawn and wired. Choosing **Open
With…** from the right-click menu on a regular file resolves the file's
absolute path (the one shared `selected_target_path` spelling), enumerates the
full `applications_for` candidate list over `RtBundleSource`, and — when at
least one application claims the type — paints it as a `lib/controls` `Menu`
(`render::build_open_with_menu`, one row per candidate in source order,
anchored where the context menu was through the shared `context_menu_rect`).
The chooser owns input while open: a primary-button press resolves through
`render::open_with_index_at` (the shared enabled-row hit-test the context menu
also uses, so paint and click cannot disagree, `AGENTS.md` §2.2) to the chosen
candidate, which is launched through the **same** `DOCUMENT_ROLE_ARG` + `STDIN`
hand-off the default open uses; `Escape` or a press off the menu dismisses it.
A file no installed application claims is stated fail-loud on `stderr` and
opens nothing — an honest "no application" answer, never an empty menu
(`AGENTS.md` §2.24). The default open still picks the first association; the
chooser lets the user pick any of them.

### File-type classification — the one content-type registry

`lib/browse::media` is the single closed registry that both the icon a tile
draws and the "Open With…" association vocabulary read, so the two can never
drift apart (`AGENTS.md` §2.2):

- `MediaType` names each content type by its IANA (or TAIRiX vendor)
  media-type spelling; `MediaType::as_str` and `MediaType::from_media_str`
  round-trip it, case-insensitively. The enum is **closed** — an unrecognised
  spelling is simply not one the registry knows (`None`, never a free-form
  string at a draw or association site).
- `media_for_name(name)` maps a filename extension to its type,
  ASCII-case-insensitively and without allocating; `media_for_entry(entry,
  parent)` classifies a listed entry — `inode/directory` for a directory
  whatever its name, `application/x-tairix-service` for a `<Name>.app` listed
  from the system service store and `application/x-tairix-app` elsewhere, and
  the extension's type for a regular file, falling closed to
  `application/octet-stream`.
- `MediaType::icon` is the glyph the type draws. That mapping is deliberately
  many-to-one and is the *only* part of the registry allowed to be: several
  distinct types share `IconKind::Text`. Two types are never merged because
  they draw alike — an application whose manifest declared the vanished type
  would silently stop matching its own files.
- `MediaType::parent` is the **subclass relation**, the same one the
  freedesktop.org shared-mime-info database models (`text/x-csrc` is a
  subclass of `text/plain`). Every readable-text type names `text/plain` as its
  broader type — `image/svg+xml` reaches it through `application/xml` — and
  everything binary names none. The chain is finite and acyclic, and
  association matching walks it, so naming a format precisely (`.rs` is
  `text/x-rust`) never narrows what can open it.

### "Open With…" — the type→bundle association

Offering a file to a chosen application is a second pure engine model, the
`open_with` module (`plans/NEW-FILEMANAGER.md` FM6b), host-proven ahead of the
app-side spawn exactly as the `Activation` decision was:

- `media_for_name(name)` derives a file's content type from its filename
  extension through the shared content-type registry above — the one bridge
  from a name (all a VFS listing gives) to the MIME vocabulary a bundle's
  signed `AppInfo` declares its associations in. An unknown or absent extension
  yields `None`, never a guess.
- `BundleSource` is the injected installed-bundle enumeration seam — the
  "Open With…" analogue of `DirectorySource`. On a running system it is backed
  by the app store (each bundle's `AppInfo` MIME table, read under the caller's
  own identity); in tests it is an in-memory list, so the matching is exercised
  without a kernel.
- `applications_for(name, bundles)` returns the `AppAssociation`s whose
  declared MIME set handles the file's type **or any broader type it is a
  subclass of** (`MediaType::parent`), so an editor declaring `text/plain` is
  offered for a `.rs` file. Candidates are ordered by how specifically they
  claim the file — an application declaring `text/x-rust` comes before one
  declaring `text/plain` — and bundles claiming at the same level keep the
  source's enumeration order. No match is an **honest empty answer** — the
  caller shows a "no application" notice (`AGENTS.md` §2.24), never a crash and
  never a fabricated default.

The type decision is a **display hint only**, like the icon classifier: it
decides which applications are *offered*, and the ordinary signed load gate
still verifies and capability-checks whichever bundle the user picks. The
engine holds no launch authority and never opens the file — spawning the chosen
bundle and the spawn-time `FdWire::Handle` file hand-off stay in the
`files.app` `Run` binary's own capability-checked tail under the user's
identity (both the default open and the explicit chooser above), so the
read-only picker composes the same engine and never launches.

### Rendering

`render(browser, theme, font, viewport, tools, tool_model, artwork)` paints a
command toolbar strip
and the current directory into a `tairix-raster` `Surface` sized to
the viewport, in whichever of the two views the browser holds
(`ViewMode::List` or `ViewMode::Grid`). The **file manager opens on the icon
grid** and its toolbar toggle switches to the list; the engine's own default
and the trusted file picker stay `List`, since a chooser wants names, sizes
and dates rather than tiles. `tools` is the manager-only
`ManagerTool` set drawn after the read-only commands — the file manager passes
`MANAGER_TOOLS`, the read-only picker an empty slice. The toolbar strip is drawn at the top
(see the frame model above); the item area sits below it
(`chrome_height` = the toolbar strip), the one header offset
the item views, the scrollbar gutter, and every hit-test share so paint and
hit-test can never disagree (`AGENTS.md` §2.2). The window title carries the
current path, so no band of the window is spent restating it. In the **list**
view each entry is a
shared `lib/controls` `TableRow` with an aligned **name / size / modified**
column layout — the same collection control (and the same one column-width
definition) the trusted picker uses, so the file manager and the picker are
one coherent themed surface rather than a browser-private row painter
(`AGENTS.md` §2.2). A name is spelled bare, whatever the entry is, and the row's
name cell carries the entry's icon — the same `icon_for_entry` classification
the grid tile draws, painted as the built-in glyph — so a folder is told from a
file by that icon rather than by a name suffix. The size column
is blank for a directory or bundle and otherwise the binary-unit `format_size`
(`1.5 MiB`); the modified column is `format_date` (an ISO `YYYY-MM-DD`, blank
at the epoch so a stampless file is never given a fabricated date, §21). In the
**grid** view each entry is a shared `lib/controls` `IconTile` — its file-type
icon over that same label, with no plate of its own, so a folder reads as a field
of icons rather than a grid of boxes and only a hovered, selected, or focused
entry paints anything behind its icon — wrapped into as many columns as fit the
width; the two views share one selection model, so toggling never moves the
selection or re-reads the directory. The icon is `icon_for_entry(entry,
parent)`: one classification both views and both consumers draw from
(`AGENTS.md` §2.2) and a
**display hint only** — it decides a glyph, never an operation; authority stays
in the VFS and the launcher. It is the shared registry's glyph
(`media_for_entry(..).icon()`, above), except that a plain directory *known* to
hold something takes the filled-folder icon instead — see *Folder occupancy*
below. `render` takes a
trailing `artwork: &mut dyn tairix_icon::IconArtwork` lookup and asks it for
each tile's icon at the exact `IconTile::icon_side` the tile reserves, blitting
the real icon artwork when the system ships and can decode it and drawing the
built-in vector glyph when it cannot — so a missing or refused asset degrades
to a meaningful icon and can never blank the tile (`AGENTS.md` §10). A tile
for an application bundle names the bundle itself in that request
(`entry_icon_request`, shared with the desktop's own icons), so `ls.app` draws
the icon `ls` carries in its own `Resources/` rather than the one generic
every-application picture. The file manager binds a real cache to that lookup
(*Grid-view icon artwork*, below); the read-only trusted picker still passes
`NoArtwork`, so it draws glyphs only.
A list row's name cell carries the same classified kind, drawn as the row
control's built-in glyph rather than through the artwork lookup. The selected
item carries the shared selection state — the raised surface plus the accent
selection rail every collection view shares — not a bespoke accent fill.

Where each item is drawn, which items are visible for the current scroll
offset, and the pixel-to-index pointer hit-test (`entry_index_at`) all come
from the one shared `layout` geometry — `ListView` and `GridView` behind the
`ViewLayout` dispatch — which clamps its scroll window through the
`lib/controls` `scroll::ScrollRange` rather than a re-derived anchor, so the
paint and the hit-test can never disagree (`AGENTS.md` §2.2). `GridView` is
**flow-parameterised** (`GridFlow`) rather than hard-wired to one direction:
`RowsFromLeading` fills a row left-to-right from the leading edge and wraps
downward, scrolling vertically in rows — the file manager's grid — while
`ColumnsFromTrailing` fills a column downward and starts each new column one
pitch inward from the trailing edge, scrolling horizontally in columns — the
[desktop icon surface](session.md#the-desktop-icon-surface). Both flows share
one cell geometry (`cell_rect`), one hit-test (`entry_index_at`), and one set
of counts (`cells_per_line`, `lines_total`, `visible_lines`, and the
`visible_range(offset)` the painter iterates), so the two surfaces cannot
drift apart (`AGENTS.md` §2.2). The tile itself is shared the same way: the
`render` helpers `grid_metrics`, `grid_tile`, and `entry_label` are public, so
the desktop paints the *same* `IconTile` — same icon side, same wrapped and
elided label, same selection state — as the file manager's grid rather than a
lookalike.

Only whole tiles are ever laid out — no icon is cut by an edge — and the two
surfaces differ in one deliberate parameter: the `GridFill` policy for the space
a line has left over once it has fitted as many whole tiles as it can. The file
manager's window is **resizable**, so its grid takes `Spread`: the leftover width
is shared out along the row, so the gaps between the tiles widen by equal amounts
and the margins at the two ends match, and widening the window past one more tile
re-flows the listing into an extra column. Only the space between the tiles
moves; a tile never stretches, so its icon slot, label field, and hit target read
the same at every window size. The pitch is the floor, so a row that fits its
tiles exactly is laid out identically under either policy, and the pixels that
will not divide into one per gap are left as the two matching end margins rather
than making one gap wider than another. The axis the grid *scrolls* along is
never spread: rows keep the fixed pitch below the header, because the space past
the last whole row belongs to the next row, one scroll away. The desktop's icon
field takes `FixedPitch` instead — it is a fixed field, not resizable content, so
keeping the pitch anchored to the edge its icons hug means an icon stays where
the user last saw it whatever the work area's exact extent is. A vertical
`lib/controls` `ScrollBar` is drawn in a reserved right-edge gutter over that
same `ScrollRange`; the wheel is routed through the shared `scroll::ScrollModel`
(`scroll_lines`), and a selection-moving key reveals the selection the least it
can (`reveal_selection`) — the browser owns the one scroll offset both consume.
`render::visible_range` is the one definition of which entry indices are on
screen, dispatching on the view mode over the same geometry both painters use,
so the folder-occupancy probe the app resolves before a frame asks about
exactly the rows it is about to draw.
The surface is rectangular; the compositor places and rounds it through its
single anti-aliased rounded-corner path, so there is no rounding in the app.
Every length saturates so a degenerate viewport paints what it can rather than
panicking (`AGENTS.md` §2.9).

### Folder occupancy

An empty folder and one that holds something draw different icons:
`IconKind::Folder` and `IconKind::FolderFilled`. A directory's `size` is `0`
and no VFS surface reports a child count, so occupancy is a separate read, and
the engine only ever draws an answer it has:

- `Entry::occupancy()` is one of four honest states — `Unprobed`, `Empty`,
  `NonEmpty`, and `Indeterminate` (a probe that was refused or failed). Only
  `NonEmpty` draws the filled folder; every other state, and every file or
  bundle, draws exactly what it drew before.
- `DirectorySource::has_children(components)` is the probe. `VfsDirectorySource`
  answers it with the cheapest honest call sequence — open the directory, read
  **one** maximal record, close — never a listing and never a walk, so the cost
  does not grow with the child count. The kernel packs a whole listing or
  refuses, so no bytes means empty, some bytes means occupied, and a listing
  too large for the one-record buffer means occupied too.
- `Browser::resolve_occupancy(range)` answers only the indices its caller
  passes, and only for an entry that still needs one. The file manager passes
  `render::visible_range`, so a hundred-thousand-entry directory probes what is
  on screen rather than what is listed. A refusal is recorded as
  `Indeterminate` and never re-asked, so a locked folder costs one probe rather
  than one per frame; a fresh listing resets every answer, so a refresh
  re-probes.
- The probe is a directory read on a child the caller is only *displaying*, so
  a source may decline it: the trait's default answers `NotImplemented`, which
  reads as `Indeterminate`. The trusted picker takes that default deliberately
  — the cue adds nothing to choosing a file — so it exercises no authority it
  does not need and its folders stay plain.

### Grid-view icon artwork

The grid draws each application's own icon, and the OS's shipped class masters
for everything else. The `Run` binary binds the renderer's `IconArtwork` lookup
to the shared artwork layer (`lib/icon::artwork`), so a tile shows the real
picture where one exists and the built-in vector glyph where it does not.
Resolution is therefore **total**: an absent, over-long, undecodable, or
disbelieved asset degrades to a glyph and never to a blank tile (`AGENTS.md`
§10, §2.9).

- **Where the assets come from.** For an application bundle, the icon its own
  signed `AppInfo` names inside its own `Resources/`; for everything else —
  and for a bundle that declares none, or whose icon will not serve — one
  `<asset-id>.png` or `<asset-id>.svg` per icon kind under
  `/System/Graphics/Icons` (`tairix_icon::icon_artwork_path`,
  `icon_vector_path`), the same store and the same spelling the
  desktop session resolves, not a file manager copy (`AGENTS.md` §2.2). The
  kind comes from the one content-type registry
  (`media_for_entry(entry, parent).icon()`), so the artwork a name gets and the
  applications offered for it can never drift apart. A bundle's manifest is
  read at that boundary as untrusted input: bounded, decoded fail-closed, and
  its icon name accepted only as a plain file name resolved inside the
  bundle's own directory.
- **How the bytes are read.** Through the app's own capability-checked VFS read
  under the launching user's identity — no new authority — bounded to one byte
  past `tairix_icon::MAX_ARTWORK_BYTES`, so an over-long asset is *detected* as
  over-long rather than silently truncated into something that looks decodable.
  It is the same bounded open/read/close the bundle-manifest scan uses, with a
  different ceiling; there is no second copy of that loop.
- **How the pixels are produced — in a sandbox, never in the app.** Icon
  artwork is a file on a volume, i.e. untrusted input, so it is decoded by the
  shared `lib/sandbox` `imagerender` service running in a minimum-capability
  worker (`AGENTS.md` §19.5). The `Run` binary re-enters **itself** in the
  reserved worker role over a fresh pipe pair — the same production launcher
  pattern the desktop session uses (`ParserSandbox` over `RtLauncher`), not a
  second mechanism — and the kernel brands that child capability-empty and
  confines it to the sandbox syscall allow-list. Nothing is decoded in the file
  manager's own address space.
- **The reply is not trusted.** The worker's echoed side and exact pixel length
  are validated by the transport, and the shared cache re-checks the block is
  exactly `side`×`side` straight-alpha RGBA8 before a surface is built from it.
  A short, long, or refusing reply resolves to `None`, which draws the glyph.
- **The fallback chain, in order.** Shipped raster artwork → the shipped vector
  asset → the built-in vector
  glyph. Every failure along the way (asset missing, asset over-long, decode
  refused, worker crashed and was replaced, reply disbelieved) falls to the
  glyph, and the refusal itself is cached so a broken asset is not re-read every
  frame.
- **Only what is drawn is decoded.** The renderer asks the lookup for the
  visible tiles alone, at the exact `IconTile::icon_side` each tile reserves, so a
  hundred-entry directory costs one read and one decode per *visible kind* — not
  per entry — and scrolling decodes only the kinds that just came into view.
  Nothing pre-warms an icon for an entry that is scrolled out of sight.
- **The memory is governed and given back.** The cache is built through the one
  shared `tairix_icon::artwork_cache` constructor with the app's real seat,
  frame size, live pressure gauge, and audit sink, so it is classified and
  budgeted by the same reclaimable-memory policy the session's caches obey — no
  hand-picked numbers. The app adds a `WaitSourceKind::MemoryPressure` member to
  the wait-set it already parks on: it reads the band once before the first
  present (the member reports only *changes*, and the process gauge starts at
  the fail-closed unknown band, which admits nothing) and, on each pressure
  wake, re-reads the band and trims the cache at the wake itself. There is no
  timer and no poll. Dropping the pipeline tears the cache down, overwriting the
  artwork first, so the pixels are released on every way out of the app — a
  window close and a fail-loud exit alike.
- **No new capability.** Hosting the sandbox worker is an ordinary restricted
  spawn, and `files.app` already requests `CAP_PROC_SPAWN` for launching an
  activated bundle; the sandbox launch reuses that same authority and asks for
  nothing more. Decoding in-process to avoid the spawn would be the wrong trade
  and is not done.

The safety properties are host-proven in `lib/browse`'s tests with fakes for
both seams (no live sandbox): artwork reaching a tile, a missing asset and an
over-long asset each reproducing the glyph frame exactly with the decoder never
reached, a wrong-length reply refused without touching the frame, one decode
across a hundred tiles, a scroll decoding only the newly visible kind, and
`teardown` dropping the charged bytes to zero.

### The places / devices sidebar

The file manager draws a vertical shortcut rail down the leading edge of its
window. The model is `lib/browse`'s `places` module and it is **pure**: it is
handed the user's home path components and a list of volumes, and returns an
ordered, validated list. It never opens, stats, or lists anything, so it is
host-testable and cannot smuggle a read past the app's own capability-checked
seam.

**Order.** One deterministic order, so the rail never reshuffles between two
paints of the same state:

1. `Home`, `Desktop`, `Documents` — the user's own places, derived from their
   home components. They are always listed, whether or not the directories
   exist (the model does no I/O and cannot know); a shortcut that turns out to
   be missing fails closed on activation and says so.
2. `Apps`, `System` — the machine's application and system roots.
3. a separation, then the mounted volumes, sorted by label.

With no home components the three home-derived rows are dropped rather than
spelled as rows that navigate nowhere.

**Volumes come from the live mount table, and the medium is real data.** The
app reads the ungated `MOUNT_LIST` query through `lib/procinfo`'s
`for_each_mount`, keeps only mounts that are actually serving I/O, and passes
each one's label, target, and `MountRecord::medium()` to the model. The icon is
`tairix_icon::disk_icon(medium)`: rotational, solid-state, and removable each
get their own shipped artwork, and a paravirtual or unreported medium gets the
generic drive glyph — never a guessed classification.

**Malformed input is dropped, not repaired.** A volume whose label is empty,
longer than `MAX_PLACE_LABEL`, or carries a control character; whose target is
not an absolute path; or whose target duplicates a row already accepted (a
fixed place included) is left out. The mount table reports what the machine has
mounted, including text this process did not author, so it is validated rather
than trusted.

**Layout and paint go through the shared machinery.** `SidebarView`
(`lib/browse::layout`) is the one definition of the rail's geometry: its width
derived from the row's icon column plus the widest fixed label measured in the
active font plus the theme's padding (never a fixed pixel count, and clamped to
a third of the window), its per-row rectangles, the separator band, and the
`index_at` hit-test that inverts them exactly.

**The command toolbar is window chrome, and the rail starts below it.**
`render::sidebar_view` lays the rail out in the window inset at the top by the
toolbar band and no taller than what is left, so the rail's first row top *is*
the first listing row's top and every rail row sits on the same `row_height`
grid as the
listing rows beside it. `render::toolbar_bounds` — and the three hit-tests that
invert it, `toolbar_command_at`, `manager_tool_at`, and `manager_tool_rect` —
take the **whole window**, because the band spans it edge to edge like the rest
of the desktop's chrome. `render::content_area` is what the
list/grid, the scrollbar, and every overlay drawn over the view occupy — the
window less the rail on the leading edge, full height — and is the rectangle
those entry points are hit-tested against, so a click resolves to the control
the user saw. With no rail it returns the window unchanged, so the trusted
picker's pixels and hit-tests are exactly as they were.

Rows are drawn with the shared `ListRow` control, which already carries the
artwork seam, so a volume shows its medium's artwork and falls back to the
built-in glyph. Every state the control offers is reachable: the pointer's row
hovers, the keyboard cursor's row is focused while the rail holds focus, the row
matching the browser's current location is *selected* through the control's own
selection state (an exact component match — standing inside a place is not
standing on it), and a row whose navigation was refused reads *disabled*.

**Input.** A primary press on a row focuses the rail, moves its cursor there,
and navigates. `Tab` moves focus between the rail and the file view from either
side; while the rail holds focus the up/down arrows move its cursor, `Enter`
activates, and `Escape` hands focus back. A place that cannot be listed leaves
the browser exactly where it was, states the reason on `stderr`, and marks the
row unavailable so it reads disabled from then on — it never wedges or blanks
the window.

**Refresh.** The kernel publishes no mount-change notification today, so the
volume rows are re-read whenever the user asks the window to refresh (`F5`, or
the toolbar's Refresh command) — the same gesture that re-lists the directory.
There is no polling loop and nothing spins waiting for a mount; the keyboard
focus and cursor survive the rebuild.

**The trusted picker draws no rail.** `render` takes the manager chrome —
write tools plus the optional rail — as one `ManagerChrome` value, and the
picker passes `ManagerChrome::none()`. That is deliberate: the picker is a
read-only chooser bounded to the tree the requesting application was authorised
to be shown, and one-click jumps to arbitrary mounted volumes would widen the
pick beyond what was asked for.

### The `Run` bundle

`files.app`'s entry point (`plans/APPWIN.md` AW3) wires `VfsDirectorySource`
over `tairix_rt::read_dir_all`, creates and grants the zero-copy window
frame region, parks on its window-event mailbox, and drives the browser
with the keyboard (`Down`/`Up` select, `Enter` activates the selection —
descending into a directory or launching a selected `<Name>.app` bundle
(spawning its own `Run` through the signed load gate, async, the launched
child reaped on the wait-set's any-child member; see *Activating an entry*),
`Backspace` climbs, `F2` renames the selected item, `Ctrl+Shift+N` makes a new
folder, and the toolbar accelerators `Alt+←/→/↑` and `F5`) and the pointer: a
primary-button press first checks the manager-only write tools
(`render::manager_tool_at` → New Folder), then the read-only command toolbar
(`render::toolbar_command_at` → the shared `apply_command`, so a click on a
disabled tool does nothing), and a press on an item selects
it (`Browser::select`) — the GUI is a spelling of the user's
intent, never an escalation, so a refused re-listing leaves the browser exactly
where it was. A `CloseRequested` from the desktop ends it cleanly, and every
bring-up refusal exits fail-loud with its reason on `stderr`. It opens at the
directory named as its first argument — the operand validated fail-closed
before any syscall, a refusal stated on `stderr` and degraded to the launching
user's home directory and then the root view, so a bad argument never leaves
the user without a window — or at that home directory when no argument is
given. The taskbar's permanent Files button spawns the bundle — or raises its
window when one is already open (`plans/NEW-TASKBAR.md` T4).

### In-place rename

`F2` renames the selected item in place. The *edit* is modelled in
`lib/browse` (`plans/NEW-FILEMANAGER.md` FM5) so it is host-tested without a
kernel; the `Run` binary supplies only the text editor and the `fs_rename`
seam. Pressing `F2` opens the one shared `lib/controls` `TextField`
(`AGENTS.md` §2.2 — never a browser-private text box) directly over the
selected row, pre-filled with the current name and bounded by the kernel's
`FS_NAME_MAX`. Typing edits the name and live-validates it: a name that
breaks a rule or clashes with an existing sibling shows the reason in the
field as you type. `Enter` commits and `Escape` abandons the edit.

The typed name is spelled through the one shared `tairix_path::validate_file_name`
rule (the same rule the browser's path components go through, `AGENTS.md`
§2.2): non-empty, not `.`/`..`, no `/`, no control character or `:`, within
the name bound. A rename to the current name is a no-op that touches neither
the VFS nor the view. A commit that survives validation is applied by
`Browser::rename_selected`, which builds the two absolute paths and calls
`fs_rename` **under the launching user's own identity — no new capability**:
the per-inode owner/mode/ACL model gates the write exactly as it would from
the shell. The whole operation is transactional and fail-closed — the name
is validated before any syscall, and a VFS refusal (a permission denial, a
read-only mount, a lost race) leaves the listing untouched and states the
kernel's reason in the field (`AGENTS.md` §2.24, §5.4), never a silent or
fabricated success. On success the directory is re-listed and the selection
follows the entry to its new name. The trusted file picker composes the same
`Browser` and simply never calls the write path, so it stays read-only
(`plans/CAPABILITY_USE.md` CU6).

### The trusted picker opens at the user's home

The desktop session's trusted file picker (the CU6 delegation UI,
`plans/APPWIN.md` AW5) opens at the logged-in user's home rather than the
storage-forest root: the session reads its `HOME` environment (exported by
login), parses it through the shared `vfs::components_from_absolute_path`, and
`open_at`s the picker's `Browser` there, falling back to `/` when `HOME` is
unset or a pick-time listing of it is refused (fail closed, never a guessed
path). So the user lands among their own files, one click from a document,
instead of drilling down from `/` every time — and the read-only picker still
composes the exact same `Browser` (it only chose a different starting
directory, `AGENTS.md` §2.2).

Opening a file into a viewer through this picker is proven end to end on the
aarch64 desktop QEMU vertical (`plans/NEW-FILEMANAGER.md` FM9-b): the session
launches the Viewer, the Viewer (handed no document) asks
the picker, the picker opens at `/Users/root`, a scripted click on the planted
document row concludes the pick, and the session delegates the chosen file to
the Viewer through the CU6 one-shot `fd_grant` / `fd_redeem` — the Viewer then
reads exactly that one file with no filesystem capability of its own. The
run's guest PASS keys on the kernel-attested `sc=fd_grant` then `sc=fd_redeem`
audit records.

### Multi-selection and the clipboard model

The management verbs — cut, copy, move, delete — act on a *set* of entries,
not just the focus cursor. That set and the cut/copy clipboard are modelled
purely in `lib/browse` (`plans/NEW-FILEMANAGER.md` FM7), host-tested without a
kernel exactly as the rename and activation models are; the app-side verbs
that execute a plan (the `fs_rename` / streamed copy / `fs_unlink`) ride on top
of it in a later increment.

`select::Selection` is the per-listing set of marked entries plus the anchor a
range extension grows from. `Browser` drives it with the familiar gestures: a
plain click or unmodified keyboard move selects one entry (`select`), a
`Ctrl`-click toggles one (`toggle_selection`), a `Shift`-click selects the
contiguous range from the anchor (`extend_selection_to`), and Select All
(`select_all`) marks everything; each bounds-checks its index against the live
listing and fails closed (`BrowseError::NoSuchEntry`) rather than marking a
phantom row. Because the members are indices into the current listing, any
listing change — a navigation, a refresh, or a re-sort — collapses the
selection back to the single focused entry, so it can never point at a stale
row.

`Browser::clipboard(op)` captures the selected entries' absolute component
paths onto a `clipboard::Clipboard` for a `Copy` or a `Cut` (`None` when
nothing is selected, so "paste" is simply unavailable rather than a silent
no-op). Because it holds absolute paths, the clipboard stays valid after the
user navigates to the directory they want to paste into. `plan_paste(clipboard,
target)` then resolves each source to a destination under the target directory
and is **fail closed** (`AGENTS.md` §5.4): a target that is one of the moved
items or lies inside it is refused as `PasteError::WouldRecurse` — an exact
root-first component-prefix test, so `/a/b` is inside `/a` but `/ab` is not —
and a paste back into an item's own directory is not silently applied but
flagged (`PasteItem::overwrites_source`) for the app to confirm or to give the
copy a new name (`AGENTS.md` §2.24). The engine only names *what* would move
where and *why a paste is refused*; the app performs the capability-checked
`fs_rename` / streamed copy under the launching user's own identity, so
composing the model grants no authority and the trusted picker never builds a
clipboard.

Given a `plan_paste` result, `execute::paste_strategy(op, source, dest)` decides
*how* each item is carried out from the clipboard operation and the two items'
`execute::VolumeId`s (the 16-byte `fs_stat` volume identity): a `Copy` always
streams, a `Cut` within one volume is a single `Rename`, and a `Cut` across
volumes is a `CopyThenDelete` — the same `st_dev` decision `mv` makes, in one
place (`AGENTS.md` §2.2). A streamed copy runs through an `execute::CopyCursor`:
it walks a known-length source in fixed `execute::COPY_CHUNK_LEN` steps,
yielding the next `execute::CopyChunk` for the app to read and write, then
`advance`s by the bytes actually carried — so a large copy holds no unbounded
buffer and never spins (`AGENTS.md` §2.23), stays cancellable between chunks,
and `resume`s from a persisted offset after a cancel or a preemption. It is
fail closed: advancing or resuming past the source length is
`execute::CopyError::Overrun` rather than a silent wrap (`AGENTS.md` §5.4), and
the source of a cross-volume move is removed only once its copy has fully
succeeded, so a failed copy loses no data. The engine does no I/O; the app
performs every `fs_rename` / `fs_read` / `fs_write` / `fs_unlink` under the
launching user's own identity, so the read-only picker never runs it.

Where an `execute::CopyCursor` streams one *file*, an `execute::CopyWalk` copies
a whole *tree* — the copy-side analogue of the delete-side `delete::DeleteWalk`.
Where a delete removes a directory's contents *before* the directory, a copy
*creates* the destination directory *before* streaming its contents into it, so
a child always has a parent to land in. `CopyWalk::from_items` begins the walk
from the resolved `(source, dest, is_directory)` items — the app supplies each
item's kind, which the path-only clipboard does not carry — and is fail closed:
an empty set, or a source or destination naming the root, yields no walk
(`AGENTS.md` §5.4). `CopyWalk::next_action` yields the next `execute::CopyAction`
— `MakeDir { dest }` (the app `fs_mkdir`s the destination directory and reports
`CopyWalk::created`), `List { source }` (the app reads the source with
`fs_readdir` and reports its children with `CopyWalk::expand`), or
`CopyFile { source, dest }` (the app streams the bytes with a `CopyCursor` and
reports `CopyWalk::copied_file`). It keeps its own explicit stack rather than
recursing on the call stack, so a deeply nested tree cannot overflow it, and it
is bounded by `execute::MAX_COPY_DEPTH` — the same `MAX_WALK_DEPTH` fail-closed
recursion bound `DeleteWalk` obeys, held in one place so the two walks cannot
disagree (`AGENTS.md` §2.2, §26.6). It holds its exact position between steps, so
the app may cancel or be preempted and resume without repeating or skipping work
(`AGENTS.md` §2.23); a deeper tree is `CopyWalkError::TooDeep` and driving it
against the wrong step is `CopyWalkError::OutOfStep`, both leaving the walk
unchanged. `CopyWalk::copied` is the honest rising count a progress indicator
shows; the total is unknown until the reads reveal it, so nothing fabricates a
percentage. The engine does no I/O; the app performs every syscall under the
launching user's own identity, so the read-only picker never runs a walk.

### The delete model

Deleting the selection (`plans/NEW-FILEMANAGER.md` FM7b) is modelled purely in
`lib/browse::delete` plus `Browser::plan_delete`, host-proven ahead of the app's
Delete verb exactly as the clipboard and paste-execution models are.
`Browser::plan_delete()` captures the current multi-selection into a
`delete::DeletePlan` (`None` when nothing is selected), one `delete::DeleteTarget`
per marked entry in listing order. Each target carries the entry's absolute
component path — so it names exactly the node the browser shows and can never
resolve to a different one (`AGENTS.md` §2.2) — and whether it is
directory-backed on disk: a directory *or* a sealed `<Name>.app` bundle, since
either is removed with `UnlinkFlags::DIRECTORY` and recursed into as the
directory it really is, while a regular file is a leaf. `DeletePlan::new` is
fail closed: an empty selection, or any target naming the filesystem root (an
empty component list), yields no plan rather than one that could remove nothing
or the root itself (`AGENTS.md` §5.4). `DeletePlan::len` and
`DeletePlan::has_directories` are the honest figures a delete confirmation
reports — the count, and whether folders (and their contents) are among the
removals — so the app's `lib/controls` `Dialog` warns truthfully rather than
treating every deletion as a single file (`AGENTS.md` §2.24). The model names
*what* would be removed; the app performs each `fs_unlink` under the launching
user's own identity — an ordinary permission-checked VFS call, no new
capability — so composing the model grants nothing and the read-only picker
never builds a delete plan.

Where the `DeletePlan` names *what* would be removed, `delete::DeleteWalk`
models *how* — the depth-first recursive removal that clears a directory's
contents before the directory itself. It is the delete-side analogue of the
paste-side `execute::CopyCursor`: a pure, host-provable driven cursor that
touches no filesystem. `DeleteWalk::from_plan` begins the walk and
`DeleteWalk::next_action` yields the next `delete::DeleteAction` — `List(path)`
(the app reads that directory with `fs_readdir` and reports its children with
`DeleteWalk::expand`, so they are removed first) or
`Remove { path, is_directory }` (the app unlinks the leaf file, or the
already-emptied directory with `UnlinkFlags::DIRECTORY`, and reports it with
`DeleteWalk::complete_removal`). The walk keeps its own explicit stack rather
than recursing on the call stack, so a deeply nested tree cannot overflow it,
and it is bounded by `delete::MAX_DELETE_DEPTH` (a fail-closed defence, not a
scaled capacity — a tree deeper than the bound is `DeleteError::TooDeep`, never
descended without limit, `AGENTS.md` §26.6, §24.4). It holds its exact position
between steps, so the app may cancel or be preempted between any two steps and
resume without repeating or skipping work — no unbounded buffer and no spin
(`AGENTS.md` §2.23). Driving it against the wrong step (an `expand` on a leaf,
or a `complete_removal` on a directory not yet listed) is
`DeleteError::OutOfStep` and leaves the walk unchanged. `DeleteWalk::removed` is
the honest rising count a progress indicator shows; the total is unknown until
the reads reveal it, so nothing fabricates a percentage. This is the browser
engine's own component-path traversal, deliberately distinct from `rm`'s
coreutils removal engine (which recurses natively over its own raw-path removal
seam with prompt/force/verbose semantics) — two consumers with two data models,
not one algorithm copied twice (`AGENTS.md` §2.2). The engine does no I/O; the
app performs every read and unlink under the launching user's own identity, so
the read-only picker never runs a walk.

The Delete verb is wired in the `files.app` `Run` binary. Pressing `Delete` on
a selection opens a modal confirmation `lib/controls` `Dialog`, built by the
shared `render::build_delete_dialog` from the captured `DeletePlan`: the title
names a single target or reports the honest count, and the message warns that
folders (and their contents) are removed when the plan includes a directory
(`AGENTS.md` §2.24). The honest Action Warmth sits on the safe **Cancel**
(recommended), never on the destructive **Delete**. `render::delete_dialog_rect`
centres and clamps the dialog to the window and `render::delete_dialog_action_at`
mirrors its button geometry so a click resolves to exactly the button pressed
(`AGENTS.md` §2.2); `Escape` (or Cancel) dismisses it, `Enter` (or Delete)
confirms. On confirm a *recoverable* removal is a move to Trash (the
move-to-Trash section below); this paragraph describes the *permanent* removal
the app falls back to when Trash is unavailable or cross-volume. It drives a
`DeleteWalk` to completion — reading each
directory with the same capability-checked listing call and shared decode the
browser navigates with, and `fs_unlink`-ing each node depth-first (with
`UnlinkFlags::DIRECTORY` for a directory-backed target) under the launching
user's own identity, no new capability. The removal is bounded and fail closed:
the first refused read or unlink stops it, states the reason on `stderr` (fail
loud, `AGENTS.md` §2.24), and leaves whatever was already removed removed rather
than a fabricated success; the view is then re-listed so a partial removal is
shown honestly (`AGENTS.md` §5.4). Only the file manager builds and drives this
— the read-only picker never deletes.

A long removal — and a long copy/paste — shows **progress** and can be
**cancelled**. Rather than driving the walk to completion in one blocking pass,
the confirmed operation is handed to an *interleaved operation* the event loop
advances a bounded slice at a time (`advance_operation`, up to
`OPERATION_STEP_BUDGET` units of work per turn — one directory read, one unlink,
one `fs_mkdir`, one copy chunk, or one rename): between slices it repaints a
modal progress panel and polls the event mailbox *non-blocking* for a mid-run
cancel or a close, so even a large recursive delete or a multi-gigabyte copy
never freezes the window and never busy-spins — continuously stepping the walk
is genuine pending work, not a spin (`AGENTS.md` §2.23). A single `Operation`
carries either a `DeleteWalk` (a delete) or a `Paste` state machine (a
copy/move), so both drive through one interleaving path (`AGENTS.md` §2.2). The
panel is the shared `lib/browse::progress` model (`ProgressModel` — the
operation kind, the honest rising `DeleteWalk::removed` / paste-node count, and
a *latched* cancel) drawn by `render::draw_progress_dialog` as a `lib/controls`
`Panel`, an indeterminate `Progress` "working" trace (no fabricated percentage,
since the total is unknown until the reads reveal it, `AGENTS.md` §2.24), and a
Cancel `Button`; `render::progress_cancel_at` mirrors the button geometry so a
click resolves to exactly the drawn Cancel (fail closed off it, `AGENTS.md`
§2.2, §5.4). The app's modal routing lives in the host-testable
`userland/apps/files/src/operation.rs`, which takes the window plus the drawn
places rail and derives the panel's rect through the same `content_area` the
panel is painted in, so the drawn button and the clickable button cannot drift
apart by the rail's width and a caller cannot hand the routing the wrong
rectangle. A cancel is latched and stops the walk at the next unit boundary —
never mid-node, and never mid-chunk — and a completed or cancelled/refused run
alike re-lists so what actually remains is shown honestly. A cross-volume move's
source-removal cleanup runs as a `Deleting` stage of the same interleaved
`Paste`, over the shared delete walk, so a move's cleanup and an interactive
delete share one removal definition (`AGENTS.md` §2.2).

### The move-to-Trash model

A delete should be reversible when that costs nothing (`AGENTS.md` §2.24). The
`lib/browse::trash` model (`plans/NEW-FILEMANAGER.md` FM10) is the pure decision
behind that recoverable delete, host-proven ahead of the app wiring exactly as
the delete and paste-execution models landed ahead of their verbs.
`trash_strategy` decides, from the item's and the user's Trash directory's
`execute::VolumeId`s, whether the removal can be a cheap `TrashStrategy::Move` —
a single same-volume `fs_rename` that carries the item into Trash intact,
recoverable until the user empties it — or must fall back to the irreversible
`TrashStrategy::Unlink`, the existing `DeleteWalk` path, when the item lives on
a different volume from Trash (a rename cannot span a volume boundary, exactly
as `mv` decides from `st_dev`). It is the same volume identity
`execute::paste_strategy` compares, so the two decisions share one definition
(`AGENTS.md` §2.2). `trash_dest_path` resolves a collision-free home *inside*
the Trash directory: the original leaf name when it is free, otherwise the
smallest ` (n)` disambiguation inserted before the extension (`notes (2).txt`),
reusing the one shared `icon` extension split so a disambiguation lands before
the same extension the icon and "Open With…" classifiers recognise. It never
overwrites an existing trashed item (`AGENTS.md` §2.24) and is fail closed: it
refuses a Trash directory that names the root (`RootTrash`), an invalid original
name (`InvalidName`), a disambiguation past the per-name length limit
(`TooLong`), and a search that exhausts the fixed `MAX_TRASH_NAME_ATTEMPTS`
bound (`NoFreeName`, `AGENTS.md` §5.4, §24.4). The model touches no filesystem
and holds no authority — the app performs the `fs_stat`/`fs_rename` under the
launching user's own identity, no new capability — so composing it grants
nothing and the read-only picker never runs it.

The **move-to-Trash verb** (`plans/NEW-FILEMANAGER.md` FM10b) is wired into the
confirmed-delete path in the `files.app` `Run` binary. Because a selection lives
in one directory — hence on one volume — a whole delete plan is uniform, so the
app decides one disposition for it *before* showing the confirmation, and the
dialog's wording matches exactly what a confirmed delete will do (`AGENTS.md`
§2.24). `begin_delete` resolves the user's home from the `HOME` the session
exported (the same source the trusted picker starts at), spells the fixed
`Library/Trash` subtree with the shared `trash::trash_dir` (one definition, so
the app and its QEMU witness agree on where a trashed item lands, `AGENTS.md`
§2.2), ensures that directory exists (`fs_mkdir` of `Library` then `Trash` under
the user's own identity), and — when the Trash and every target share a volume —
resolves each target's collision-free `trash_dest_path`. If all of that holds
the removal is a recoverable **move to Trash**: `render::build_delete_dialog` is
built with `trash::DeleteDisposition::Trash` (a safe, recommended *Move to
Trash* action and a "you can restore them" message), and on confirm the app
drives a `Job::Trash` operation that renames each target into its captured Trash
destination — one `fs_rename` per item through the same interleaved
progress/cancel runner as a delete or paste (`ProgressOp::Trash`), so even a
large selection stays responsive and cancellable (`AGENTS.md` §2.23). Anything
that makes the move impossible — an unset or root `HOME`, a Trash directory that
cannot be created or stat'd, or a cross-volume target (a mounted volume under
the current directory) — falls back, fail closed, to the irreversible
`DeleteWalk` unlink, and the dialog is built with `DeleteDisposition::Permanent`
(the destructive *Delete Permanently* action and the "cannot be undone"
warning), so the user is never promised a recovery the removal will not honour.
Every step is the launching user's own §5.3-checked call — **no new
capability**, no ambient authority.

The **empty-Trash model** (`plans/NEW-FILEMANAGER.md` FM11a) is the irreversible
counterpart of the move above — now that the move fills the Trash, permanently
emptying it is no longer speculative surface (`AGENTS.md` §2.4).
`trash::empty_trash_plan` turns an `fs_readdir` of the Trash directory into a
`delete::DeletePlan` over its *contents* — never the Trash directory itself, so
emptying leaves the now-empty folder in place — carried out by the same
recursive `DeleteWalk` a permanent delete already uses, so there is no second
removal engine (`AGENTS.md` §2.2). Emptying is always permanent, so the app
confirms it with `DeleteDisposition::Permanent`. It returns nothing to do for an
already-empty Trash (a no-op the app simply does not offer, not an error) and is
fail closed: a root Trash directory (`RootTrash`) or an invalid child leaf
(`InvalidName`) refuses the whole empty rather than remove outside Trash or
silently skip an item (`AGENTS.md` §5.4). Like the move, it touches no
filesystem and holds no authority — the app drives the plan with its own
`fs_readdir`/`fs_unlink` under the launching user's identity — so composing it
grants nothing and the read-only picker never builds one.

The **empty-Trash verb and the Trash view** (`plans/NEW-FILEMANAGER.md` FM11b)
are wired into the `files.app` `Run` binary through two manager-only toolbar
tools (see the frame model above), which the file manager hands to `render` and
the read-only picker never draws. **Go to Trash** (`ManagerTool::Trash`) is the
navigable Trash location: it resolves the user's home from `HOME`, ensures the
`Library/Trash` subtree exists (the shared `trash::trash_dir`, one definition,
`AGENTS.md` §2.2), and navigates the browser there with `Browser::navigate_to`
so the Trash's contents are shown like any other directory. **Empty Trash**
(`ManagerTool::EmptyTrash`) renders disabled — muted, never hidden — unless the
current directory *is* the user's Trash and it is non-empty (a `ManagerToolModel`
the app computes from `HOME`, since the engine does not know it); when it is
enabled, clicking it re-reads the Trash, builds `trash::empty_trash_plan`,
confirms it with the `DeleteDisposition::Permanent` dialog, and — on confirm —
drives the plan's `DeleteWalk` through the same interleaved progress/cancel
runner an ordinary delete uses (`ProgressOp::Delete`), under the launching
user's own `fs_readdir`/`fs_unlink` (no new capability). A stale click recomputes
the Trash location and refuses to empty anything else (fail closed, `AGENTS.md`
§5.4).

The whole empty-Trash flow is proven end to end on the production desktop by
the autoload QEMU vertical (`plans/NEW-FILEMANAGER.md` FM11c, appended after
FM10's move-to-Trash delete): the runner clicks **Go to Trash** to navigate the
front files window into `Library/Trash` (now holding the trashed folder),
clicks **Empty Trash** to open the *Delete Permanently* confirmation, and
clicks its Delete button — every point reconstructed from the app's own layout
code (`render::manager_tool_rect` over the whole window for the tools;
`trash::empty_trash_plan` → `render::build_delete_dialog` with
`DeleteDisposition::Permanent` → `Dialog::action_rects` for the confirm button,
the same code the guest paints and hit-tests with, `AGENTS.md` §2.2). The
guest's PASS gate latches on the kernel's own `FsNodeMutated op=rmdir` audit
record whose target is under
`Library/Trash`, observed only after the FM10 move has latched, so no earlier
removal can satisfy it (fail closed). The empty burst is held behind a one-shot
serial marker the test kernel emits the first time it observes the move latch,
so the clicks land only once the folder is provably in the Trash and Empty
Trash is enabled.

For the file manager to find the user's Trash, the desktop session hands its
launched apps the **user environment** login exported. Plain `spawn` gives a
child an empty environment; the session's `spawn_app` helper instead launches
the file manager, terminal, and viewer with `spawn_with`, forwarding its own
environment (so `HOME`, `LANG`, … are inherited exactly as a login shell's
children inherit them) under the session's attested credential and console —
the environment is data and carries no authority (`AGENTS.md` §4, §5.4).

### The cut / copy / paste verbs

The clipboard verbs (`plans/NEW-FILEMANAGER.md` FM7b) are wired in the
`files.app` `Run` binary on top of the FM7a/FM7b engine models above. The app
holds one `clipboard::Clipboard` in its overlay state, captured by
**`Ctrl+X`** (a move clipboard) or **`Ctrl+C`** (a copy clipboard) from the
current selection (`Browser::clipboard(op)`); with nothing selected the verb is
simply unavailable (fail closed). Because the clipboard holds absolute paths it
survives navigating to the paste target. **`Ctrl+V`** pastes it into the
current directory: `plan_paste` validates the plan (a paste of a folder into
itself is refused outright and nothing is enqueued), the app stats the
destination directory for its `execute::VolumeId`, and hands the plan to an
interleaved `Paste` operation the event loop then carries out a bounded slice at
a time (see the progress + cancel description above), so a large copy never
freezes the window. Every item runs under the launching user's own identity —
**no new capability**, every operation an ordinary §5.3-checked VFS call the
user could perform themselves. `execute::paste_strategy` chooses the mechanism
per item from the two nodes' volume ids: a same-volume move is one `fs_rename`,
a cross-volume move is copy-then-delete (the source removed through the shared
delete walk, as a `Deleting` stage of the same interleaved `Paste`, only once
its copy has fully succeeded), and a copy streams — a single file through an
`execute::CopyCursor` and a directory (or sealed `.app` bundle) through an
`execute::CopyWalk`, both driven over `fs_read`/`fs_write`/`fs_mkdir`/`fs_readdir`
with one reused, fixed-size (`FS_IO_MAX`) buffer so a copy of any size holds no
unbounded buffer and never spins (`AGENTS.md` §2.23, §26.6). It is bounded and
fail closed: the first refused operation stops the paste, states the reason on
`stderr` naming the item (fail loud, `AGENTS.md` §2.24), and leaves whatever
already landed in place rather than a fabricated success (`AGENTS.md` §5.4); the
view is then re-listed so a partial paste is shown honestly. Initiating a `Cut`
paste clears the clipboard (its sources are being moved, so re-pasting the same
cut would name items that are gone); a `Copy` keeps it for another paste. A
destination is created **exclusively**, so a pre-existing item of the same name
is refused rather than clobbered, and a `Copy` back into an item's own directory
is refused rather than silently duplicating a file onto itself (`AGENTS.md`
§2.24) — overwrite/merge confirmation is a separately-staged follow-up. Only the
file manager builds and drives this; the read-only picker never pastes.

### The new-folder model

Creating a folder (`plans/NEW-FILEMANAGER.md` FM7b) is modelled purely in
`lib/browse::mkdir` plus `Browser::create_directory`, host-proven ahead of the
drawn New Folder tool exactly as the rename model landed ahead of its editor.
`validate_new_dir_name` spells the typed name through the one shared
`tairix_path::validate_file_name` rule — the same rule the rename editor and
every path component obey (`AGENTS.md` §2.2) — and refuses a name a sibling
already carries (`MkdirError::Clash`); both are decided *before* any syscall,
so a rejected name touches neither the VFS nor the view.
`Browser::create_directory` spells the new folder's absolute path through the
same shared `spell_child` the launch/open targets use, so the create can never
name a different node than the browser shows, then applies it through an
injected `fs_mkdir` seam under the launching user's own identity — an ordinary
permission-checked VFS call, no new capability (`AGENTS.md` §4, §5.3). It is
transactional and fail closed: a VFS refusal (a read-only mount, the user
cannot write the parent, a lost race) leaves the listing exactly as it was and
is surfaced as `MkdirError::Refused` for an honest in-UI answer (`AGENTS.md`
§2.24, §5.4). On success the directory is re-listed and the selection follows
onto the new folder, ready for the app to open its inline rename editor over
it. The model reads nothing and holds no authority, so the trusted picker
composes the same `Browser` and never calls the write path.

The drawn tool is wired (see the frame model above): New Folder is a
manager-only `chrome::ManagerTool` the file manager hands to `render` (the
picker does not), reachable by clicking the toolbar tool or pressing
`Ctrl+Shift+N`. The `Run` binary names a non-clashing placeholder with
`mkdir::suggest_new_dir_name`, creates it through `Browser::create_directory`,
and opens the inline rename on the new folder so the user names it at once.

The whole New-Folder + inline-rename flow is proven end to end on the
production desktop by the autoload QEMU vertical
(`plans/NEW-FILEMANAGER.md` FM9-a, appended after the AW4 terminal round
trip): the runner refocuses the served files window, descends into
`/Users/root` by coordinate-computed pointer clicks — reconstructing the
browser's own row layout through `render::selection_rect` over the real
listings and the New Folder tool through `render::manager_tool_rect` over the
whole window (the band the toolbar is drawn across), the same layout code the
guest paints with, offset by the window manager's client inset
(`WindowFrame::insets`) so a click lands on the client, not the decoration
(`AGENTS.md` §2.2) — and seat-keyboard `Enter`s, clicks the New Folder tool,
and types a distinct name. The guest's PASS gate latches on the kernel's own
`FsNodeMutated` audit records — `op=mkdir` then `op=rename`, observed after the
terminal round trip so no boot- or login-time directory creation can satisfy
them — so the mkdir and the rename are kernel-attested, under the logged-in
user's own identity, and a refused mutation (`FsMutationDenied`) can never
count (fail closed).

The **confirm-and-remove** flow (`plans/NEW-FILEMANAGER.md` FM9-c) is reached
by right-clicking the selection and choosing **Delete**, which runs the same
`begin_delete` → confirm-dialog → `DeleteWalk` path the `Delete` key drives.
Making the right-click reach the app required a compositor fix that makes the
*whole* context menu usable in the desktop: secondary (right) button presses
were being dropped, so `tairix_wm`'s input router now raises+focuses and
delivers a client-area right-press as `InputResponse::SecondaryActivated`, the
desktop session's router forwards it to the window manager, and the session
delivers `WindowEvent::Pointer` `Pressed(Secondary)` to the app — host-tested
in `tairix-wm` and `tairix-desktop-session`. The shared `Menu::row_rect` and
`render::context_menu_command_rect` give a caller the drawn Delete row's rect
(`AGENTS.md` §2.2).

The earlier note that a scripted right-click "never arrives in the guest" was a
test-harness bug, not an emulator limit, and is now fixed and proven. QEMU's
HMP `mouse_button` help string ("1=L, 2=M, 4=R") is wrong: `hmp_mouse_button`
maps state bit `0x2` to the right button and `0x4` to the middle (the legacy
`MOUSE_EVENT_*` `bmap`). The QEMU test harness (`tools/qemu`) had trusted the
help string and sent a secondary press as bit `0x4`, so QEMU delivered a
*middle*-button event and no OS layer ever saw a right-click.
`MouseButton::mask_bit` now sends `0x2` for the secondary button, and a
dedicated aarch64 vertical
(`tairix-test-pointer-button-virtio-mmio-qemu-aarch64`) proves it: it attaches
a `virtio-mouse-device`, injects a secondary press+release, and asserts the
driver decodes `BTN_RIGHT` (`0x111`), never the middle button (`0x112`) — it
times out with the old mask and passes with the fix.

The full right-click → Delete → confirm click-through is now wired into the
aarch64 `autoload_input` vertical end to end. Appended after the FM9-b Viewer
open-a-file, it is gated on the Viewer's `sc=fd_redeem` serial line (the last
FM9-b event, and the image's only `fd_redeem`), so it runs strictly after the
CU6 delegation without depending on the app-ward delivery counter (the FM9-b
Viewer window delivers its own focus event, leaving that count statically
unknown). The runner right-clicks the FM9-a folder row (the secondary press
raises+focuses the files window over the frontmost Viewer and opens the context
menu on the folder), clicks the drawn **Delete** row, and clicks the
confirmation dialog's Delete button — every point reconstructed from the app's
own layout code (`render::selection_rect`, `render::context_menu_command_rect`
over the menu built exactly as the app builds it, and `render::delete_dialog_rect`
with the dialog's own `action_rects`), never a hand-copied coordinate
(`AGENTS.md` §2.2). A tenth guest PASS witness latches from the kernel's
`FsNodeMutated op=rmdir` audit record, gated on the FM9-b delegation being
redeemed so no earlier removal can satisfy it (fail closed — a refusal logs
`FsMutationDenied`, a different id) — so the manager's right-click delete is
kernel-attested under the logged-in user's own identity, non-flaky across
repeated runs.

### The properties model

The Properties panel (`plans/NEW-FILEMANAGER.md` FM8) shows one selected node's
metadata. That view is modelled purely in `lib/browse::properties`, host-tested
without a kernel exactly as the rename, activation, and clipboard models are;
the drawn panel that paints it is described below.

`Properties::from_stat(name, kind, stat)` turns an entry's name, its browser
`EntryKind`, and the `fs_stat` `FileStat` the app read for it into the
display-ready fields the panel renders: a human kind label (`Folder` / `File` /
`Application` — the `EntryKind` distinguishes a sealed `<Name>.app` bundle from
an ordinary directory), the apparent `size` and on-disk `allocated` bytes (both
via the shared `format_size`, never one derived from the other), the raw mode
and its four-digit octal spelling, the ten-character permission string
(`drwxr-xr-x`), the owning uid/gid, and the four `Time64` stamps rendered as
`YYYY-MM-DD HH:MM:SS` through `format_datetime`. Every field comes straight
from `fs_stat`; a stamp the backing does not keep is `Time64::UNIX_EPOCH`,
which renders blank rather than as a fabricated `1970-01-01` wall time
(`AGENTS.md` §21). The permission string is the one shared
`tairix_abi::fs::mode_string` spelling — the same definition `ls -l` renders —
so the two can never disagree on what a mode means (`AGENTS.md` §2.2); the
permission string's leading type indicator reads from the structural
`FileStat::kind`, so a bundle is *labelled* "Application" yet honestly shows a
directory's `d`. The model reads nothing and holds no authority: the app
performs the one capability-checked `fs_stat` under the user's own identity and
hands the result here, so the trusted picker composes the same view.

### The drawn properties panel

The drawn overlay (`plans/NEW-FILEMANAGER.md` FM8b) paints that model as a
shared `lib/controls` `Panel` centered over the current view, so it is one
coherent themed surface rather than a browser-private box (`AGENTS.md` §2.2).
`render::properties_rows` is the one definition of *which* fields appear and how
each reads — Kind, Size (apparent plus on-disk), Permissions (symbolic plus
octal), Owner, and the four timestamps — host-tested so the drawn panel and its
tests can never disagree about the field set; `render::draw_properties` draws
the panel titled with the node's name and lays those rows out as muted-label /
solid-value columns, clipping so a window too small for the whole panel shows
what fits rather than panicking (`AGENTS.md` §2.9). The panel reads only the
already-authorised `Properties` and holds no authority.

The `files.app` `Run` binary opens the overlay with **`Alt+Enter`** on the
selected item: it names the item's path through the shared
`Browser::selected_target_path` spelling and reads its metadata with one
capability-checked `fs_stat` under the user's own identity (`fs_open` with the
directory flag for a folder or sealed bundle, read-only for a file — `stat`
needs only a live handle), then shows the panel. While the overlay is open it
owns the window — **`Escape`** dismisses it and every other keystroke is
swallowed rather than navigating the view behind it. Showing properties is an
incidental, refusable action: if the item can no longer be named or its
metadata cannot be read (it vanished, or is unreadable), the refusal is stated
on `stderr` and the overlay stays closed — an answer, not a crash, and never a
fabricated summary (`AGENTS.md` §2.24, §5.4).

### Editing permissions

The permission-edit *model* (`plans/NEW-FILEMANAGER.md` FM8b) is
`lib/browse::mode_edit` plus `Browser::set_mode_selected`, host-proven ahead of
the drawn permission control exactly as the properties view model landed ahead
of the drawn panel. `validate_mode` fails closed on any bit above
`tairix_abi::fs::FS_MODE_MASK` — the settable `rwx`/setuid/setgid/sticky word —
refusing it rather than masking it into a lesser mode, so the mode committed is
always exactly the one asked for and never silently a different one (`AGENTS.md`
§2.24, §5.4). `Browser::set_mode_selected` names the selected node through the
shared `Browser::selected_target_path` spelling, validates the mode *before*
any syscall, and applies it through an injected `fs_set_mode` seam under the
user's own identity — an ordinary permission-checked VFS call, no new
capability (`AGENTS.md` §4, §5.3). A VFS refusal (the user does not own the
node, a read-only mount, a lost race) leaves the node's mode exactly as it was
and is surfaced as `ModeError::Refused` for an honest in-UI answer (`AGENTS.md`
§2.24). The directory listing carries no mode, so a successful change re-reads
nothing; the app re-stats the node to refresh the panel. The model reads
nothing and holds no authority, so the trusted picker composes the same
`Browser` and never calls the write path.

### The drawn permission control

The file manager's Properties overlay is *editable*: `render::draw_properties_editable`
draws the metadata fields as the read-only `render::draw_properties` does, then
below them a **labelled permissions grid** — `Read`/`Write`/`Exec` column
headers over three `Owner`/`Group`/`Other` triad rows of clickable
`lib/controls` `Checkbox` toggles reflecting the current mode. The grid replaces
an earlier cramped single-row layout in which nine boxes were placed a single
glyph apart (so they overlapped) with no label; each toggle now sits on a real
grid pitch under its own row and column label. `render::PERMISSION_BITS` and
`permission_cells` are the one definition of which of the nine
owner/group/other bits each toggle carries, and `render::permission_cell_at` is
the mirror hit-test returning the bit a click flips (and nothing off a toggle,
fail closed). The shared `render::PermGrid` geometry places the painted grid,
its headers/row-labels, and the hit-test from one definition, so a click always
lands on the box it depicts (§2.2). Only the write-capable file manager draws
the editable overlay; the trusted read-only picker draws `draw_properties` and
never resolves a toggle, so the write surface is separated from the picker by
call site, not a runtime flag — the same discipline as the manager-only write
tools (`AGENTS.md` §2.2). The editable popup is the taller
`render::properties_editable_panel_rect` (the read-only picker keeps the
shorter `properties_panel_rect`) so the grid has its own room; the browser
window is resizable (see *Resizable window* below), so a user may enlarge it
further, and the grid fails closed — drawing nothing rather than off-panel —
only when the window is genuinely too small (§5.4).

A primary-button press on a toggle flips only that `rwx` bit — preserving the
current setuid/setgid/sticky bits (the settable word masked by `FS_MODE_MASK`,
dropping the non-settable file-type bits `fs_stat` also reports) — and commits
the new mode through `Browser::set_mode_selected` over `fs_set_mode` under the
user's own identity. On success the overlay is re-stat'd so it shows the
applied mode; a refusal is stated on `stderr` and leaves the node's mode
exactly as it was (`AGENTS.md` §2.24, §5.4). The setuid/setgid/sticky bits stay
visible in the panel's octal and symbolic spelling and are edited through the
`chmod` command — a deliberate scope boundary for a bloat-free panel, not an
omission.

### Editing ownership

The ownership-edit *model* (`plans/NEW-FILEMANAGER.md` FM8b) is
`lib/browse::owner_edit` plus `Browser::set_owner_selected`, host-proven ahead
of the drawn ownership control exactly as the permission-edit model landed
ahead of its control. It is deliberately unlike the other write verbs (rename,
mode, mkdir), which are the user's own §5.3-checked writes needing no new
capability: reassigning a file's **owner** is a privileged operation, so it is
gated by a dedicated capability, `CAP_FS_CHOWN` — the Unix `CAP_CHOWN`
analogue, carried by the administrator ceiling and by nothing an ordinary
session holds (`AGENTS.md` §5.2). The whole authority rule lives kernel-side in
the secured VFS behind the new `fs_set_owner` syscall: reassigning the uid, or
setting a group the caller is not a member of, requires `CAP_FS_CHOWN`;
otherwise only the node's owner may change the group, and only to a group they
already belong to (the unprivileged `chgrp`). Any successful change clears the
set-user-ID bit (and the set-group-ID bit of a group-executable node — a
set-group-ID directory keeps it), so a reassigned file can never carry a stale
set-*id* escalation, the standard `chown(2)` safety behaviour.

The engine models none of that policy. `OwnerChange` names *what* to change —
each of `uid`/`gid` is either `None` (leave unchanged) or `Some(id)` (set) —
and `validate_owner` fails closed on a field set to the reserved
`FS_OWNER_UNCHANGED` sentinel as an explicit target, refusing it before any
syscall rather than misreading it as "unchanged". `Browser::set_owner_selected`
names the selected node through the shared `Browser::selected_target_path`
spelling, validates before any syscall, maps `None` onto the sentinel, and
applies through an injected `fs_set_owner` seam under the user's own identity;
a VFS refusal — including the `PermissionDenied` a caller without `CAP_FS_CHOWN`
receives — leaves the ownership exactly as it was and is surfaced as
`OwnerError::Refused` for an honest in-UI answer (`AGENTS.md` §2.24, §5.4). The
listing carries no ownership, so a success re-reads nothing; the app re-stats
the node to refresh the panel. The model holds no authority, so the trusted
picker composes the same `Browser` and never calls the write path.

### The drawn ownership control

The file manager draws an editable ownership control on the Properties
overlay's owner row, **but only where the launching user holds
`CAP_FS_CHOWN`** — read once from the kernel-attested `self_origin` at
start-up, so a session that cannot reassign ownership is never shown a control
it cannot use (`AGENTS.md` §2.24). `render::draw_owner_control` overlays the
uid and gid values of the read-only `render::draw_properties` owner row: an
accent underline marks each value as clickable, and while one is being edited
the shared `lib/controls` `TextField` is drawn over it. `render::OwnerField`
and `render::owner_field_at` are the mirror hit-test resolving a click to
exactly the uid or gid value it edits — measured from the same `uid N / gid N`
spelling the panel draws, so the drawn control and the hit-test can never
disagree (§2.2) — and a click off a value resolves nothing (fail closed, §5.4).
Like the permission control, the write surface is separated by call site (only
the file manager calls `draw_owner_control`; the trusted read-only picker never
does), *and* additionally gated on the runtime capability, since owner
reassignment is privileged.

The `files.app` `Run` binary opens the inline id editor on a click, pre-filled
with the current id and bounded to a `u32`'s ten digits, live-validates the
typed value, and on `Enter` commits through `Browser::set_owner_selected` over
`fs_set_owner` under the user's own identity (the kernel enforces `CAP_FS_CHOWN`
and the group-membership rule); `Escape` cancels. A non-numeric or
out-of-range id, or a VFS refusal — including the `PermissionDenied` a caller
without `CAP_FS_CHOWN` receives — states its reason in the field and keeps the
editor open, an honest answer rather than a silent or fabricated result
(`AGENTS.md` §2.24, §5.4). On success the panel is re-stat'd so it reflects the
new owner.

### Resizable window

The file manager opens its window `resizable`, so the window manager presents
it with a live maximize/restore size toggle and a resize grabber (a fixed-size
app is offered neither). On a `WindowEvent::Resized` the `Run` binary re-maps
its zero-copy frame region at the new client size and re-presents; the shared
`lib/browse` renderer lays the toolbar, listing, and scrollbar out to
whatever viewport it is handed, so the content fills the new size with no
per-size layout code. The re-map is fail-closed (`AGENTS.md` §5.4): a fresh
region is allocated and granted and adopted only once the session accepts
`WindowClient::resize`; the old region is released only after adoption, and a
refused or unallocatable resize leaves the current window intact rather than
blanking or crashing. `MIN_WIN_WIDTH` × `MIN_WIN_HEIGHT` is *declared* on the
window create (`WindowSizing`) and enforced by the window manager, so a drag
simply stops there. The app must not clamp a granted size itself: resizing its
own window back up while a drag keeps shrinking makes the two fight once per
pointer sample, which is what made the listing visibly bounce as the window
approached its minimum. An app never answers a resize with a larger size of
its own. The default window is sized so the editable Properties popup
(metadata plus the permissions grid) fits without resizing.

Icon-only buttons size their glyph from the plate itself — the smaller plate
dimension inside its frame, less a small margin proportional to the plate
(`lib/controls` `icon_content_side`) — rather than from the text inset
(`control_inset`, tuned to keep a *line of type* clear of the frame). On the
default 28px control plate the text inset left only a ~6px glyph adrift in the
button (the "tiny icon" defect); the proportional margin instead fills roughly
three-quarters of the plate. This is the one `lib/controls` icon-button paint
path, so every icon-only button across the desktop (the file manager's
navigation and manager-write tools, window controls, the taskbar) benefits
(`AGENTS.md` §2.2).

## Terminal emulator (`tairix-terminal`)

The terminal emulator hosts the system shell and shows its output on a
character-cell screen rendered through the active theme. Like the browser it is
split into a screen **model** and a **renderer**, both driven by an injected
shell I/O seam, so the parsing and rendering logic is testable without a kernel
(`AGENTS.md` §7).

### The shell seam

`ShellSource::read()` returns the bytes the shell has produced since the last
call (an empty read is not an error) and `ShellSource::write(bytes)` forwards
the user's keystrokes. On a running system the seam is
`spawned::PipeShellSource` (`plans/APPWIN.md` AW4): two kernel pipes to a
shell child the terminal spawned under its own `CAP_PROC_SPAWN`, wired at
spawn through `spawned::shell_wires` — the child's stdin is the keystroke
pipe and its stdout *and* stderr land on the one output pipe a terminal
renders (fd 3 is closed; advisory records are best-effort by contract).
Reads drain one bounded chunk per wait-set wake and surface end-of-stream as
the typed "shell has exited" refusal; writes loop over short writes and fail
closed on a wedged channel. The process-spawn authority lives in the `Run`
binary, behind the seam, not in the screen model.

### The screen model

`Grid` is a fixed `cols`×`rows` rectangle of `lib/vt` `Cell`s — a glyph plus
its folded `Attributes` — with a cursor and a rendition pen. It exposes the
cursor-relative operations a terminal needs: writing a glyph with the pen
(wrapping and scrolling at the edges), the C0 moves, absolute/relative cursor
positioning, the ANSI erase operations, the scroll region and explicit
scrolling, the alternate screen, cursor visibility, the saved cursor, the
window title, and clear.

`Parser` is a thin **consumer** of the shared `lib/vt` ANSI/VT/xterm
vocabulary (`plans/CURSES.md` C2): it lets `lib/vt`'s streaming parser turn
shell output bytes into the shared `Op` vocabulary and applies each `Op` to
the grid, so there is exactly one escape-sequence definition in the tree, not
a second divergent one (`AGENTS.md` §2.2). The emulator is xterm-class —
printable text and Unicode, the C0 controls, SGR rendition with the
16/256/truecolour colour models, cursor addressing, the erase operations, the
scroll region (`DECSTBM`), the alternate screen (`?1049`), cursor visibility
(`?25`), the saved cursor (`ESC 7`/`ESC 8`), and the OSC window title — and it
honestly advertises `xterm-256color` because every capability that name
implies is really parsed (the compiled-in capability database is the next
`plans/CURSES.md` stage, `lib/termcap`). Because `lib/vt`'s parser is total, an
unrecognised, oversized, or malformed sequence is consumed without disturbing
the screen, so an unfamiliar stream degrades to dropped control rather than a
corrupted display or a panic (`AGENTS.md` §2.9).

`Terminal` ties the grid, the parser, and the seam together: `pump` reads the
shell's output and applies it to the screen, and `send` / `send_str` forward
input. The terminal never echoes input itself — echo, line editing, and job
control are the shell's responsibility, exactly as on a real tty — and a
failing seam call surfaces the boundary `Errno` while leaving the screen
unchanged (`AGENTS.md` §5.4).

Because "echo, line editing, and job control are the shell's responsibility"
presumes a real tty *between* the emulator and the shell, the correct shell
channel is a **pseudo-terminal** whose slave carries the kernel's tty line
discipline (local echo, cooked line editing, `CR`/`LF`→`CR LF` on both
directions, `Ctrl-C`/`Ctrl-Z` job control), reusing the same discipline the
hardware console runs — never a second copy (`AGENTS.md` §2.2). That pty is
staged in `plans/PTY.md`; the environment-forwarding half (below) has landed.

### The size it opens at

A terminal's natural size is a character count, not a pixel count, so the
window is whatever the conventional 80×25 screen (`layout::COLS` ×
`layout::ROWS`) measures in the face actually being drawn with — the face's
own advance and line height, resolved from the font service at runtime, never
a compile-time constant. The window furniture is allowed for through the one
shared `WindowFrame::insets` definition the compositor decorates with
(`layout::chrome_insets`), so the app and the window manager cannot disagree
about how much room a decorated window needs.

When a display cannot hold that grid, the **text size** gives, never the grid:
`layout::fit_font_size` steps the profile's size down a logical pixel at a
time until the 80×25 screen plus its furniture fits, stopping at
`MIN_FONT_SIZE_PX`. A terminal that quietly dropped to 60 columns would break
every program that lays itself out for 80. The default 14-logical-pixel size
puts the framed window inside a 640×480 display with room left for the
taskbar, and a denser display multiplies it through the desktop scale. The
staged design is `plans/GUI-TERMINAL.md`.

### Rendering

`Screen` owns a `tairix-raster` `Surface` the size of the window's client
area and **keeps it between frames**, using the shared `tairix-font`
monospace family (Inconsolata EX plus the M PLUS 1 Code Japanese, D2Coding
Korean, and Noto Sans Hebrew companions). Hebrew and Yiddish letters,
final forms, punctuation, and marks occupy individual terminal cells;
Japanese and precomposed Hangul full-width bitmaps paint their lead and
continuation cells as one unit, so a continuation-cell background cannot erase
half a glyph.
Each cell is drawn with its own rendition: its
`lib/vt` `Attributes` select the foreground and background, resolved one way
through the user's colour scheme — a `Default` colour takes the scheme's own
foreground / background, the 16 basic colours and the low 16 palette entries
take the scheme's ANSI slots, the 6×6×6 cube and greyscale ramp above them are
the fixed xterm arithmetic no scheme reinterprets, truecolour is used
directly, `reverse` swaps the pair, and `bold` brightens a basic colour. The
visible cursor cell is drawn as the scheme's cursor block. The surface is
rectangular; the compositor places and rounds it through its single
anti-aliased rounded-corner path, so there is no rounding in the app. Every
length saturates so a viewport smaller than the grid paints what fits rather
than panicking.

#### A repaint costs what changed, not a window

`Screen::paint` holds the cells the surface was last painted from and
compares the grid against them, so a frame redraws only the block that
differs and returns it as the rectangle to present. A keystroke costs the
cell it wrote and the two the cursor moved between — a couple of glyph
blits, a couple of cells copied into the shared frame, and a two-cell
damage rectangle for the session to recomposite — rather than re-rendering,
re-converting, and re-presenting the whole window for one character. A wake
that changed nothing presents nothing at all. Two equal cells paint
identically only under the same colours and the same face, so a profile,
theme, or scale change calls `Screen::invalidate` and the next paint covers
the window. A resize needs no such call: the present step reconciles the
picture to the display mode describing the shared frame region, so the two
can never disagree, and reshaping invalidates implicitly.
A damaged block is widened to whole glyphs before it is drawn, so
overwriting the continuation half of a wide glyph repaints its lead cell
too. The equivalence that makes this safe is a unit test: an incremental
repaint must land pixels byte-identical to a fresh whole-window paint of the
same grid, across typing, cursor moves, scrolling, rendition changes,
erases, and wide-glyph clobbering.

### Colour schemes, the profile, and screen effects

`scheme.rs` is the one place a terminal colour comes from: a `ColorScheme` is
the sixteen ANSI slots plus background, foreground, cursor, and cursor text,
and `Painted` resolves the scheme in force once per repaint rather than once
per cell. **System** follows the desktop's own dark/light appearance and is
the default; **Midnight**, **Phosphor**, **Amber**, **Ember**, **Contrast**,
and **Paper** carry fixed palettes; **Custom** is the user's own, editable in
the settings sheet.

Everything a user can change is one `Profile`, stored per user at
`/Users/<u>/Settings/Terminal/terminal.conf` in the same `key value` / `#`
comment grammar every line-oriented TAIRiX configuration store shares. An
absent document means the documented defaults (a fresh account has never saved
one); an unreadable or malformed one also means the defaults, and says why on
`stderr`. Colours are written as bare `rrggbb` because the grammar's comment
marker would cut a `#`-prefixed value away.

The screen effects are an ordered, typed pipeline (`effects::Pass`) rather
than code inlined into the renderer, so a display that can composite hardware
layers can programme its own engine from the same description with the
software passes staying the conformance oracle. Translucency is not a pass at
all: the default background is filled at the profile's alpha, so the
compositor's own premultiplied blend shows the desktop through while a glyph
stays opaque. Backdrop blur is the compositor's (`set_backdrop_blur`), since
only it can see behind a window. Scan lines, fuzz, phosphor persistence, and
wobble run over the finished frame. An animated effect is a pure function of a
monotonically increasing `Phase` that the program advances on a one-shot frame
deadline in its wait-set park — there is no poll loop, and a terminal with the
effects off never wakes for them at all.

A pass is a *whole-frame* post-process by nature — wobble displaces rows and
phosphor decays every pixel — so when one is in force the finished screen is
copied into a reused buffer, the passes run there, and the whole window is
presented. The retained screen itself stays clean, so the next frame's cell
diff still describes the text rather than the effect's own churn, and an
animated terminal re-runs its passes without re-rendering the grid. That
buffer exists only while an effect is on. Translucency and backdrop blur are
not passes, so a see-through, frosted terminal still types at cell-diff
cost.

### The menu and the settings sheet

A secondary (right) press opens the terminal's context menu: *Settings…*,
*Larger text*, *Smaller text*, *Actual size*, *Clear screen*, and *Close*,
each with a keyboard shortcut that really works whether or not the menu is
open. The popup is the shared `lib/controls` `Menu`, placed by that control's
own `anchored_rect` rule — the same placement the file manager's context menu
uses.

Hovering it costs what it changes. The shared control reports the rows it
redraws into a damage sink, and reports **nothing** for a sample that leaves
the highlight where it already was, so `ContextMenu` answers `Ignored` for it
and the program neither re-renders its plate nor republishes the popup's
window. Sliding the pointer within one row is therefore free; crossing into
another repaints once. The settings sheet answers the same way on the same
seam, so resting the pointer on a control is free too — and a round that *did*
report keeps its whole-plate repaint, because a change the sheet composes above
its controls (a switched tab's body) is wider than the rectangle the control
that caused it reports.

*Settings…* opens an in-window modal sheet built from the shared Reactive
Alloy controls: an **Appearance** tab (the scheme chooser, the text-size
slider, and the custom scheme's twenty colour wells with red/green/blue
sliders) and an **Effects** tab (one slider per effect). Every edit clamps
through `Profile::clamp`, re-derives the colours and the face, reshapes the
grid — the pty window size follows, so the shell re-lays-out — and writes the
document.

### The `Run` bundle

`terminal.app`'s entry point reads the user's profile, creates the pty, spawns the user's default
shell (`tairix_users::policy::DEFAULT_SHELL`) forwarding its own **inherited
environment** to the shell (`USER`, `HOME`, `LOGNAME`, `PATH`, `LANG`, …, with
the emulator's own `TERM` replacing any inherited one — the shared,
host-tested `spawned::shell_env` rule), so the shell's prompt and its children
run under the logged-in user's identity and locale instead of the anonymous
`user@host` fallback, exactly as the desktop session forwards the environment
to every app it launches. The child-side pipe ends are closed after the spawn
(so each side observes the other's end-of-file honestly). It then creates and
grants the zero-copy window frame
region, and **parks** on one wait-set with three members — its window-event
mailbox (`Port`), the shell-output pipe's read end (`Stream`, the AW4 kernel
addition: ready on buffered bytes or end-of-stream), and the shell child
(`Child`) — dispatching on the woken member's token, never a poll loop. The
park carries a one-shot frame deadline only while an animated screen effect is
in force. A key press is claimed by an open menu or settings sheet, else by a
terminal accelerator, else encoded through the one shared `lib/keymap` rule
and written to the shell (releases send nothing); shell output is pumped into
the grid and the repainted frame presented. The shell exiting, the user
choosing *Close*, or a `CloseRequested` from
the desktop, ends the session cleanly; every bring-up refusal exits
fail-loud with a reserved code and its reason on `stderr`. The desktop's
program-library popup lists the terminal's catalog entry, which spawns the
bundle (`plans/NEW-TASKBAR.md` T5), and
the autoload QEMU vertical types a real command into the served window at
the seat keyboard, PASSing only on the kernel-attested keyboard → session →
terminal → pipe → shell → spawn round trip.

## File viewer (`tairix-viewer`)

The read-only text viewer is the first consumer of the desktop's trusted
file picker and the CU6 one-shot file delegation (`plans/APPWIN.md` AW5,
`plans/CAPABILITY_USE.md`). Its manifest requests `CAP_CONSOLE_WRITE` and
`CAP_SHM` and deliberately **no filesystem capability**: on its own the
viewer can open, list, and stat nothing.

At startup the `Run` binary creates its window and immediately asks the
session's picker (`WindowClient::pick_file`). The user browses in the
*session's* UI under the *session's* authority; the viewer receives
exactly one conclusion on its authenticated event channel — a
`FilePicked` carrying the kernel's one-shot `fd_grant` handle, or a
`PickCancelled`. Redeeming the handle (`fd_redeem`, unprivileged)
installs a read-only descriptor whose reads the kernel re-authorises
under the session's captured identity, so the viewer reads exactly the
one file the user chose and nothing else — the user-mediated file
capability of `AGENTS.md` §16.5, end to end.

The host-tested view engine keeps untrusted content honest:
`content_lines` bounds the shown bytes (`CONTENT_MAX`), splits on line
feeds, and sanitises **every** non-printable byte to a placeholder before
anything reaches the renderer, so a hostile picked file can neither pin
unbounded memory nor smuggle control sequences; `render_status` /
`render_lines` paint through the active theme and the shared monospace
face.

The window is mouse-driven, exactly as every desktop app is expected to
be. A `Viewer` composes the current file view (or a status message) with
a shared header `Button` ("Open…") and a shared vertical `ScrollBar`,
kept in step through one `ScrollModel` — the same scroll geometry the
window manager's own root-viewport bars use. `ViewerLayout` is the one
definition of where the header, the button, the text area, and the
scrollbar's gutter sit within the window, shared by rendering and pointer
routing so the two can never disagree about where a control actually is.
`Viewer::on_pointer` is the single pure entry point a translated
`tairix_input::InputEvent` is fed into: clicking the button asks for
another pick, dragging the thumb or clicking the track scrolls to the
dragged or paged offset, and the wheel steps the view — the `Run` binary
translates each delivered wire pointer event into that vocabulary through
the one shared `tairix_window::pointer_input_events` mapping, the same
translation the file manager's window channel uses. The keyboard remains
a fully working secondary path: `Enter` asks for another pick, and the
arrow/page/home/end keys still step the view. A cancelled pick leaves the
viewer open with a notice; a `CloseRequested` ends it cleanly. The program
library's catalog carries the viewer's entry, so the desktop's popup
spawns the bundle.
