# NEW-TASKBAR.md — the taskbar / icon bar becomes first-class

Binding under `AGENTS.md`. This plan records the completed Stage 7 taskbar
(`userland/gui/taskbar`, `tairix-taskbar`), a floating **icon bar** with:

- a permanent left-most **Program Library** launcher — a first-class,
  folder-organised catalog of installed applications (Accessories,
  Programming, Games, Internet, …), programmatically add/removable exactly
  as an installer adds shortcuts;
- to its right, a permanent **File Manager** icon that opens `files.app`
  on its default view;
- then the **application strip**: one icon-only slot per *running
  application*, each slot standing for one kernel-attested process. A primary
  click performs the application's own declared default action (or, for one
  that declared none, raises its most recently used window); hovering a slot
  whose application owns more than one window opens the window picker; and a
  secondary press opens the menu that **the application itself** declared,
  whose *Info* row shows a session-drawn information panel of its bundle's
  **signed** manifest. There is no pinning: applications are launched from
  the program library or from a desktop shortcut;
- on the right, the **notification area**;
- and, always right-most and immovable, the **Switchboard** icon — the
  system-overview surface that implements `plans/desktop1.png` and
  `plans/desktop2a.png` and is where System Settings is reached.

Read first, in order: `AGENTS.md` (all of it, especially §2, §5, §10,
§16, §17, §24, §26, §27), `plans/GUI-CONTROLS-DESIGN.md` (the Reactive
Alloy `lib/controls` vocabulary every surface here composes — **no second
control implementation**, §2.2), `plans/APPS.md` (bundle model, command
resolution, `lib/cmdres`), `plans/APPWIN.md` (AW2 window channel, AW5
`lib/browse` engine, the CU6 one-shot delegation), `plans/NEW-FILEMANAGER.md`
(the `files` app), `plans/DISPLAY.md`
(the seat/display model), `plans/FIX-DESKTOP.md` (non-blocking async launch),
and `plans/CAPABILITY_USE.md` (CU6 capability sizing). Every rule in all of
them applies here without exception.

**Note:** `abi-v1` is *not* frozen (the standing task direction supersedes
the `AGENTS.md`/`PLAN.md` freeze language until first release). A `lib/abi`
change today is allowed; it requires regenerating the C header
(`cargo xtask c-header --write`), which the drift guard enforces.

## Status

`done` — **T1–T16 complete**, including T15's documentation deliverable and
its QEMU icon-bar vertical, and T16's desktop icon surface.
Each stage's done-state section below records what it now guarantees. The
**Switchboard tray** is landed whole
(T9/T10): the immovable trailing-most capsule slot with the
`SwitchboardTray` model (one pure derive, hung > pressure > jobs >
recovery > calm, orthogonal furniture composed, count/alert badge), the
hover instrument readout, scroll task-cycling and the middle-click
previous-task switch, the seat-scoped `SWITCHBOARD_ENDPOINT` +
`switchboard_ipc` summary vocabulary (`lib/abi`, fuzzed), the session's
attested relay + `HangTracker` delivery-evidence hang detection, and the
`userland/gui/switchboard` monitor service (tickless sampler over
`lib/procinfo`, change-only publisher with keepalive, capability-sized
manifest, spawned by the session at bring-up and calm-on-death). The
library **data** layer is landed end to end: `lib/proglib` (T1 — taxonomy,
entry model, store grammar, fail-closed parse, canonical render, `merge`,
`reconcile`, fuzzed by `tests/fuzz_proglib.rs`), the `applib` admin command
(T2 — `userland/apps/applib`), the manifest `library` listing + `applib
rescan` discovery, and the image-build catalog seeding (T3 —
`tools/mkimage`). The library **UI** layer is landed too: the two permanent
leading launchers and the program-library popup (T4/T5); the generic start
menu is gone. The **application strip** is landed whole (T6/T7 — see their
done-state sections below): the bar's per-application slots, the
`SetAppBar`/`AppBarDefault`/`AppBarMenu` window-channel contract an
application declares its own presence and menu through, the hover window
picker, the session's grouping of windows under their attested owner with the
manifest-attested slot identity, and the sandboxed per-app icon pipeline
(`lib/image` PNG + `lib/compress` inflate/zlib + the `lib/sandbox`
icon-rasterisation service). The **notification
area** is first-class too (T8 — see its done-state section below): the
versioned, fuzzed `notify_ipc` channel (`lib/abi`) over a seat-scoped
`NOTIFY_ENDPOINT` the kernel binds only for the desktop's live seat lease,
the taskbar's typed status signals + severity-ranked transient-notification
cards (shared `lib/controls`), and the session serving the endpoint —
attesting each producer, relaying raise/clear, presenting the
click-to-dismiss popover, and dropping a dead producer's notifications on
exit. The rest of the starting point is Stage 7 as it stands:
`tairix-taskbar` models the
launchers / popup / application strip / window registry / picker /
notification area / clock and emits
typed `TaskbarResponse`s; `tairix-session` presents the bar, popup, and
menu through the compositor, owns the theme, loads/merges the catalog
stores, and resolves those responses (`plans/FIX-DESKTOP.md` async launch
is done); the Switchboard app's own screen composition
(`userland/gui/switchboard::view`) already renders a `SwitchboardModel`
→ `SwitchboardAction` from the shared Reactive Alloy controls; the `files`
app is a live windowed browser (`plans/APPWIN.md` AW3/AW5). This plan wires
the remaining pieces together and fills the gaps.

## 0. Scope and decisions (binding for this plan)

- **One control implementation, ever (§2.2).** Every visible surface here —
  the library launcher, the file-manager button, the application slots and
  their hover picker's cells, the
  notification tray, the Switchboard icon, and the whole Switchboard window —
  is composed from the shared Reactive Alloy controls in `lib/controls`
  (`Button`/`IconButton`, `Menu`/`MenuItem`, `ListRow`/`Card`/`Panel`,
  `TaskbarItem`, `TraySignal`, `Notification`, `Dialog`, `ScrollBar`, and the
  WM furniture), resolved from `lib/theme` + `lib/geometry`, drawn through the
  single `lib/raster` fill and `lib/font`/`lib/icon`. **No taskbar-private
  widget, no second rounded-corner path, no application-painted chrome.** A
  new visual the mockups imply (a meter bar, a pressure rail, an activity
  seam) is added to `lib/controls` as a shared control, never hand-rolled in
  the taskbar or the Switchboard app.

- **The taskbar holds no authority; the session/services do (§4, §5, §17.4).**
  `tairix-taskbar` is a `userland/gui/*` model+render crate depending only on
  `lib/*`. It **cannot** spawn processes, read privileged system state, or
  control other tasks. Every interaction produces a **typed response**; the
  capability-holding component resolves it: the session glue
  (`tairix-session`) launches bundles (via `appmgr`, async — `plans/FIX-DESKTOP.md`)
  and owns the theme; the Switchboard service (below) holds the
  `CAP_SYSINFO_*` / process-control authority. Authority is always enforced by
  the existing capability-checked syscall/IPC paths; a control only renders
  state and *suggests* an action (`plans/GUI-CONTROLS-DESIGN.md`).

- **Nothing is a compiled-in list (§16.5, §18.5).** The set of applications the
  Program Library shows is **data on disk**, discovered from the installed
  `.app` bundles and an editable catalog store — never a hard-coded table in
  the kernel, the image builder, a test fixture, or the taskbar. Adding an app
  to the library is dropping/registering a bundle on the volume, exactly as
  adding a driver is dropping a signed bundle in the driver store.

- **The Switchboard is its own process, not part of the session (CU6 sizing,
  `plans/APPWIN.md` §0).** Hosting the system-overview inside `tairix-session`
  would force the session's manifest to grow `CAP_SYSINFO_GLOBAL`,
  `CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_HW`, and process-control authority — the
  opposite of the CU6 minimum-capability rule. Instead a dedicated
  **Switchboard component** (T10–T13) is a self-contained signed bundle with
  exactly its own manifest ∩ ceiling set. The session stays the D7 graphical
  class (`CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM`) plus the relay glue.

- **Live data comes only from the System Information API (§16.6).** The
  Switchboard's meters, task/job/pressure/recovery/overview state are sourced
  through `lib/procinfo` / the `sysinfo` queries under `CAP_SYSINFO_*`. There
  is **no** `/proc`/`/sys` scrape and no private back-channel (§16.1, §17.3).

- **Fail closed and fail loud (§2.9, §2.24, §5.4).** A missing catalog, an
  unreadable bundle, a denied action, a degenerate screen size, or an
  absent Switchboard service degrades to an empty/quiet surface with a stated
  reason (logged, or shown in-UI for an interactive refusal) — never a panic,
  never a fabricated result, never ambient authority.

- **No `for now`, no stubs (§2.19, §27).** Each stage lands its surface
  *fully* — every state `plans/GUI-CONTROLS-DESIGN.md` §11 gives the controls
  it composes, dark/light + high-contrast + reduced-motion, complete
  pointer/keyboard/focus behaviour, and tests — or, if a stage is genuinely
  too large, its landed part is complete and the remainder is staged here and
  surfaced (§15.7). A foundational primitive (the catalog engine, the bounded
  icon-bar menu model, the tray-signal feed) is the complete abstraction, not
  its first caller's slice (§27).

## 1. Final bar layout (left → right, horizontal bottom bar)

The bar floats at its configured edge. `Metrics::taskbar_margin` is `5`
logical pixels in both built-in themes, scaled through `Scale::scale_length`
and applied to the three sides facing the screen edge; the fourth side faces
the work area and keeps the bar's thickness. A too-small screen clamps the
margin and still lays out the bar.

The bar is drawn as the shared floating surface plate
(`tairix_controls::paint_surface_plate`, the recipe every popup it opens also
wears): a rim one `plate_border` thick in the palette's `rim` tone, then the
ground inside it, both at the chrome weight below and both rounded by
`taskbar_corner_radius` — the same radius the compositor cuts the bar window
to, so the rim follows the silhouette rather than squaring off across it. The
rim reads a step lighter than the ground on a dark theme and a step darker on a
light one, and stays see-through.

Every region the bar lays out sits *inside* that rim: `BarLayout::compute`
places the bar's own rectangle, then lays the content out through a placer
pulled in by one `plate_border` on both axes, so a hovered or pressed slot's
plate cannot wash over the surface's edge. `BarLayout::bar` is still the whole
rectangle, rim included. A bar too thin to spare two rims keeps its content
rather than the inset.

The bar is handed its theme already in the floating form the session derives
once for all of its chrome (`DesktopSession::floating_theme`, which grounds every
menu plate too), so the bar, its program-library panel, hover window picker,
notification popover, and Switchboard readout — and every control drawn on any
of them — are floating chrome by construction, with nothing told separately and
nothing left an opaque patch. Each surface keeps the colour role it wears solid
and takes the palette's `chrome_alpha` (four fifths): the bar and the readout
ground in `surface_raised`, the two panels in `surface`. A plate raised on one
— a launcher's hover wash, the library's search field, the readout's *Open
Switchboard* button, a notification card — takes the step-more-solid
`chrome_plate_alpha`, while a row, a menu row, and the scroll channel take the
ground's own. Marks stay solid: an accent highlight, a rail, a bead, a rim, a
focus ring, every icon and label. The session requests `chrome_backdrop_blur` —
`7` logical pixels in both built-in themes — behind each surface, which is what
the translucency reads against.

An application slot has a hover look but no pressed one, unlike the two
launcher buttons, which compress. Whether the strip should state a held slot
at all is an open question for the furniture spec
(`plans/GUI-CONTROLS-DESIGN.md`), not a decision to make in the renderer.

Along-bar popup placement is clamped to the bar's own span, so no popup enters
the wallpaper gap. Floating panels draw no separate header band.

```
wallpaper gap  ┌──────────────────────────────────────────────────────────┐  wallpaper gap
               │ [Library] │ [Files] [app] [app] [app] …        │ [Switch]   │
               └──────────────────────────────────────────────────────────┘
                                      wallpaper gap
```

- **Leading, fixed order, not reorderable:** `Library` (Program Library
  launcher) then `Files` (file manager). These two are permanent and cannot be
  removed or moved: they are *launchers*, not slots.
- **Separator rule:** the rule after `Library` is the only one the bar
  actually paints — the other `│` above are group boundaries in this sketch.
  It is one `border_thickness` along the main axis (floored at one physical
  pixel), inset one `control_inset` from both long edges so it clears the
  bar's rounded ends, in the palette's `border` colour, with one `control_gap`
  either side. `Files`, the application strip, and every trailing region begin
  one whole gutter past `Library`, which puts the file manager on the applications' side
  of the rule rather than the library's. It is decoration:
  `BarLayout::hit_test` has no case for it, so a press on the rule reaches the
  bare bar. Laid out once as `BarLayout::separator` and drawn from that rect,
  never re-derived by the painter (§2.2); `Rect::EMPTY` — and so unpainted —
  when the bar is too short to reach it or too thin to inset it.
- **Application strip:** one `TaskbarItem` per *running application*, in the
  order the session first saw each process, after the permanent launchers.
  Empty when nothing is running. A window is reached through the hover picker
  a slot opens, never through a slot of its own (T6/T7).
- **Notification area:** status icons + transient notifications, left of the
  clock; the clock sits between it and the Switchboard icon (desktop1
  panel 1). A secondary press on the clock opens the clock's own menu (T17);
  a primary press on it is claimed and inert, as on a status signal.
- **Switchboard icon:** always the trailing-most element, reserved, immovable;
  no application or tray icon may occupy or displace its slot.
- Vertical / top / right edges reflow along the cross axis by the existing
  `Edge`/`Orientation` model; "left/right" above is main-axis leading/trailing.

The generic start menu is retired (T4 — done): the leading icon is the
**Program Library launcher**, and the session controls (Log Out, Lock, Shut
Down, Restart) and the appearance (light/dark) toggle arrive in the
**Switchboard's system quick-actions menu** (desktop1 panel 5, T13) — until
then theme switching is programmatic (`DesktopSession::set_theme`).

## 2. Crates and layering (§17.4)

New / changed homes, all obeying the one-way `userland/gui/* → lib/*` edge:

- `lib/proglib` **(new, `no_std`)** — the shared **program-library catalog
  engine**: the folder taxonomy, the entry model, the on-disk store grammar,
  the fail-closed bounded parser, the canonical render, and the machine ∪ user
  overlay merge. Modeled exactly on `lib/sysconfig` (grammar + closed registry
  + fail-closed parser + render, no I/O, no authority). Consumed by the
  installer, the `applib` admin command, and the taskbar/session. (T1)
- `lib/abi` — extend `AppInfo` with the optional `library` listing (the
  opt-in folder byte + `library-icon` asset, and the `purpose`/`author`
  fields the information panel states) so the library is *discovered* from
  bundles (T3); add the taskbar↔Switchboard **tray-signal summary** record and
  the **library-edit** / **icon-bar** / **Switchboard-control** IPC
  vocabularies under the usual ABI discipline (versioned, hashed, fuzzed).
- `lib/controls` — add the shared controls the mockups need that do not yet
  exist (the **MetricTile** resource reading, **PressureRail**,
  **ActivitySeam**, **SignalBead**
  refinements) so both the taskbar icon and the Switchboard window compose
  them (T9, T11–T14). The Switchboard app's own screen composition is
  extended (Pressure + Activities sections) in place (§2.13).
- `userland/gui/taskbar` — the leading library button, the application
  strip with its declared menu and hover picker, the reserved Switchboard
  slot, and the richer notification area (T4, T6, T7, T8, T9).
- `userland/gui/session` — the glue: launch library/files bundles, hold every
  application's icon-bar declaration and relay its outcomes, build the
  picker's thumbnails, present the library popup, forward Switchboard
  open/reveal, relay the tray-signal summary to the taskbar (T4–T9).
- `userland/gui/switchboard` **(new)** — the Switchboard component: a
  long-running monitor service that samples the system and publishes the
  tray-signal summary + serves the on-demand overview window, whose screen is
  this application's own composition over the shared controls (T10–T13).
- `userland/system/applib` **(new command app)** — the first-class CLI for
  programmatic library add/remove/list, the installer's peer (T2).

## 3. Folder taxonomy (the library's top-level folders)

The taxonomy is a **closed, curated set** in `lib/proglib`, chosen to match
the well-understood freedesktop.org main menu categories so third-party
packagers already know where their app lands. It is a Rust enum
(`LibraryCategory`), so an app declaring a category that is not in the set is
rejected at catalog-write time (fail closed), and adding a category is a
reviewed one-line data change, never free-form text:

| Folder | Holds |
|---|---|
| `Accessories` | Calculator, text editor, clock, notes, archive tools |
| `Graphics` | Image viewers/editors, screenshot |
| `Internet` | Browser, mail, chat, remote access |
| `Multimedia` | Audio/video players, recorders |
| `Office` | Documents, spreadsheets, PDF |
| `Programming` | Editors/IDEs, terminals used as dev tools, debuggers |
| `Games` | Games |
| `SystemTools` | Monitors, disk tools, task shells (**not** System Settings) |
| `Utilities` | Small single-purpose tools that fit no folder above |
| `Other` | Catch-all for a bundle that declares no category |

- **System Settings is deliberately absent (issue requirement).** Settings is
  reached through the Switchboard → System quick-actions menu (T13), never as
  a library folder. A bundle whose category resolves to a settings surface is
  refused from the library catalog.
- Command apps (`/System/Commands`, `plans/APPS.md` §8), background services
  (`/System/Services`), and any bundle whose manifest declares no `library`
  folder never appear in the library — listing is an explicit manifest
  opt-in, so only user-facing graphical applications do.
- Folder display order is the enum order above; entries within a folder sort
  by display name (locale-aware, deterministic). An empty folder is hidden.

## 4. Capabilities (§5.2 — introduced with their enforcement point, never ahead)

Each capability below is added **only** in the stage whose service both holds
and enforces it; none is defined speculatively (§2.4, §5.2). Before adding
any, the implementer checks whether an existing capability already expresses
the authority at the right granularity and, if so, uses it.

- The **machine-wide** catalog write under `/System/Settings/ProgramLibrary/`
  mints **no capability** (resolved in T2): no `CAP_SETTINGS_WRITE` exists in
  the tree, and §5.2/§16.2 forbid defining one ahead of the settings service
  that would hold and enforce it. The enforcement point is the §5.3 per-inode
  policy the kernel VFS already applies under the caller's attested identity
  — the store is a system-owned file an ordinary account reads but cannot
  rewrite, and the kernel logs the denial. The **per-user** overlay likewise
  needs no new capability: since `plans/APPDATA.md` AD10 it lives in
  `applib`'s *published* app-data scope, gated on the bundle identity the
  kernel attests for that program rather than on anything the caller holds.
- `CAP_SYSINFO_GLOBAL` / `CAP_SYSINFO_KERNEL` / `CAP_SYSINFO_HW` — **existing**
  (§16.6); the Switchboard component requests them to read the live overview.
  No new capability is minted for reading.
- Process-control authority for the Switchboard's task actions (pause/resume,
  quit/force-quit). The check for an existing capability is **resolved**
  (owner-approved with T9/T10): the `signal` syscall's vocabulary already
  carries the needed actions (`Continue`/`Stop`/`Terminate`/`Kill`,
  `lib/abi/src/process.rs`) but its target rule is own-child-only, and no
  existing capability expresses cross-principal process control — so **T11**
  widens the `signal` target rule in place to the kill(2)-style "own child,
  else same-uid, else `CAP_PROC_CONTROL`" and mints
  `CAP_PROC_CONTROL` **in that same change**, where its live holder (the
  Switchboard manifest; the administrative ceiling in `lib/users`) and its
  live enforcement point (the widened kernel signal dispatch) land together
  — never ahead of them (§5.2). "Lower priority" needs new scheduler
  surface and stays in T12 where it is used. Every control action is a
  capability-checked syscall under the Switchboard component's own identity
  — never ambient (§4) — and every allow/deny is audit-logged (§19.4).
  An ordinary user's Switchboard controls only that user's own processes
  (the same-uid rule, no capability needed); the capability exists for the
  administrative overview acting across principals.
- Machine-power authority for the quick-actions menu's **Restart** and
  **Shut Down** rows. The check for an existing capability is **resolved**
  (T13): no capability expressed it — `CAP_PROC_CONTROL` reaches other
  principals' *processes* but never the platform, `CAP_SEAT_ADMIN`
  administers seats on a machine that stays running, `CAP_DRV_KERNEL` loads
  code rather than ending execution — so T13 mints `CAP_SYSTEM_POWER` (id 41)
  together with its live enforcement point, the new capability-gated
  `system_power` syscall (number 105), and its live holder, the Switchboard
  service manifest plus the administrative ceiling in `lib/users`. It guards
  a class (the machine's power state), not one object, and is granted in the
  administrative ceiling only, so an ordinary account's desktop renders the
  power rows with the Authority Mark and never attempts them.
  - **The desktop session deliberately does not hold it.** The session is the
    largest, most exposed process on the seat — it composites, parses input,
    decodes untrusted image assets, and serves IPC — so the widest-blast-radius
    authority in the system stays out of it. The session relays a confirmed
    power request to the small Switchboard service, which performs the syscall
    under its own capability check and reports a refusal on `stderr`.
  - **Authority is attested by the holder, never guessed by the renderer.**
    The Switchboard publishes whether it actually holds `CAP_SYSTEM_POWER` in
    the tray summary it already sends; the session passes that through to the
    taskbar, which renders the rows accordingly. An absent, dead, or
    not-yet-published service leaves the rows **denied** — fail closed, never
    optimistic.
- Session **Lock** re-authentication mints **no capability**, and no second
  authenticator is written. The per-console elevation broker
  (`lib/abi/src/elevate.rs`, served by the login supervisor, which already
  holds `CAP_SPAWN_AS_USER` + `CAP_USERS_READ`) already performs exactly this
  work — timing-equalised re-authentication through the same `Authenticator`
  the login prompt uses, indistinguishable refusals, per-attempt audit, secret
  zeroisation. T13 therefore adds one **narrower** request kind to that
  protocol, `ElevateRequest::Verify`, which verifies the **caller's own
  kernel-attested uid** and runs nothing. It is strictly weaker than the
  `Run` request the same endpoint already serves, so it widens no authority.
  Callers reach it through the one shared client, `tairix_rt::elevate`, which
  derives the console from the caller's own kernel-attested origin and erases
  the request buffer on every return path.

## 5. On-disk stores (data, never code)

- **Machine-wide catalog:** `/System/Settings/ProgramLibrary/library.conf`
  (writable `/System/Settings` subtree, `nosuid,nodev,noexec`, §16.2). A
  plain `lib/appconf` `key = value` document whose *registry* `lib/proglib`
  owns: each entry a fail-closed record of `bundle-path`, `category`,
  `display-name`, optional `icon` asset id, keyed by a stable `id`. Written
  only by principals the `/System/Settings` per-inode policy admits (the
  system identity). It stays a file because it is *machine* policy rather
  than any one application's data.
- **Per-user overlay:** `applib`'s **published** app-data scope
  (`tairix_proglib::LIBRARY_PUBLISHER`, `plans/APPDATA.md` AD10) — same
  registry; lets a user hide, re-file, or rename an entry, and add entries
  for their own `/Users/<u>/Applications` bundles, without touching the
  system store. It moved off `/Users/<u>/Settings/ProgramLibrary/library.conf`
  because every application that account launched could read *and rewrite*
  that file: a hostile program could file a launcher row named "Terminal"
  against a bundle of its choosing. `applib` is now the only principal that
  can write it, and the session reads it through the one foreign-read shape,
  which carries no scope field. Merge policy: the user overlay is applied
  over the machine
  catalog (user hide/rename/re-file wins; user-only entries append). The
  merge is one pure, exhaustively-tested function in `lib/proglib`.
  The application strip has no store at all: it is derived from live state —
  the declarations the window engine attested and the windows each process
  owns — on every wake it can have changed, so there is nothing about it to
  keep in step with a document.
- Both are **untrusted input** to every reader: bounded length, alloc
  discipline per crate policy, fail closed on anything not fully understood
  (unknown key, bad category, duplicate id, oversize), and a reader that
  cannot fully parse runs on an empty store rather than guessing (§2.9, §5.4,
  §24.4 — these are format bounds, not growable capacities).

---

# Staged plan

Each stage is independently reviewable, ends green on the whole-project gate
(§7), and lands its surface fully (§27). Stages are ordered by dependency;
T1–T3 (library data), T4–T5 (library UI), T6–T7 (the application strip and
the app→bar contract), T8–T9 (tray + icon),
T10–T13 (Switchboard), T14 (fidelity), T15 (docs/gate), T16 (the desktop icon
surface). T9 needs the T10 tray-signal feed for its live states, so the two
land together.

## T1 — `lib/proglib`: the program-library catalog engine — **done**

`lib/proglib` (`no_std` + `alloc`, stability `experimental`) is the catalog
engine every later stage builds on. What it now guarantees:

- **The taxonomy** — `LibraryCategory`, the closed folder set with
  locale-neutral ids and a total, deterministic presentation order.
- **The entry model** — `LibraryEntry` over the validated newtypes `EntryId`
  / `DisplayName` / `BundlePath` (confined to an application bundle inside an
  application store, parsed through `lib/path`) / `IconAsset`, so an invalid
  entry is unrepresentable.
- **The store** — the `<id>.<field>` line grammar (keys split at the *last*
  `.`, so a reverse-DNS id is valid) over the closed `EntryKey` registry
  (`name`, `bundle`, `category`, `icon`, `hidden`); a bounded, fail-closed
  `parse` that refuses the **whole** document (`CatalogError` with the
  offending line) and a canonical `render` that round-trips exactly. A
  declaration may carry its own `hidden true` suppression — the record
  keeps its identifier claimed, so a rescan cannot resurrect it — and a
  patch carries the overlay's visibility verdict (`false` re-shows).
- **`Catalog`** — `insert`/`patch`/`remove`/`get`/`entry`/`entry_patch`,
  `records`/`entries`/`patches`, and the `folder`/`folders` views, failing
  closed with `CatalogFull` at `MAX_ENTRIES`. A record is a declared
  `Record::Entry` or an overlay `Record::Patch`.
- **`merge(machine, overlay)`** — the one pure overlay resolution: overlay
  entries replace machine entries of the same id, patches apply machine-first
  so the user's verdict — visibility included — wins field by field (hiding
  is presentation, never authority), an entry whose resolved verdict is
  hidden is dropped, and a patch naming no entry is discarded (so
  re-installing restores the personalisation).
- **`Catalog::reconcile(discovered)`** — the self-healing discovery fold
  (T3): declares every discovered entry whose identifier no existing record
  claims and never disturbs curation, refusing the whole fold at
  `MAX_ENTRIES`.
- **The store spellings defined once** — `LIBRARY_DIR`, `LIBRARY_FILE`,
  `LIBRARY_SETTINGS_SUBDIR` and `LIBRARY_PATH` for the machine layer, and
  `LIBRARY_PUBLISHER` — the bundle identifier a reader hands to
  `tairix_appdata::read_published` — for the overlay. No I/O, no authority.

Docs: `lib/proglib/README.md`, `docs/src/lib/proglib.md`, `AGENTS.md` §3,
`PLAN.md`. Tested host-side beside the code (round-trip, every fail-closed
rejection, ordering determinism, merge precedence, empty-store default) and
fuzzed by `tests/fuzz_proglib.rs`, registered with `cargo xtask fuzz`.

## T2 — Programmatic add/remove: the `applib` command app — **done**

The first-class "installer adds a shortcut" path (issue requirement).
`userland/apps/applib` (a command app lives under `userland/apps/`, §3; GNU
conventions per §16.7) now guarantees:

- **The grammar** — `applib [list [--category <folder>]]`,
  `applib add <bundle> [--category <f>] [--name <n>] [--icon <a>] [--user]`,
  `applib remove <id|bundle> [--user]`, `applib hide|show <id> [--user]`,
  `applib rescan [--user]`; `--opt value` and `--opt=value`, `--`
  end-of-options, and the reserved `-h`/`-?` short-help switches. `add`
  derives id/name/folder/icon from the bundle's own signed `AppInfo`
  (overridable); an unlisted manifest without `--category` is refused, never
  guessed.
- **One engine, no authority** — every document is read/written whole
  through `lib/proglib`, over two backings of one `Store` seam so the tool's
  editing logic never learns where a catalog lives. The machine store goes
  through the secured VFS under the caller's attested identity and is gated
  by its §5.3 per-inode system ownership (no new capability — §4 above; the
  kernel logs the denial); `--user` targets the caller's own overlay in
  `applib`'s published app-data scope, which the service resolves from the
  attested bundle identity, so no path names it and no `HOME` is needed to
  reach it. `hide`/`show` record the visibility verdict on the target
  store's own entry or as an overlay patch.
- **Fail loud, fail closed** — refusals name their store side and reason on
  `stderr` (§2.24) with GNU-style exit codes (0/1/2); a malformed store
  refuses the whole operation; nothing partial is ever written.
- **`stdinfo` records (§20.1)** — one `summary` record per completed change
  on fd 3 (`apps.library_entry_added`/`_removed`/`_hidden`/`_shown`,
  `apps.library_rescan`), JSON-escaped, best-effort, never load-bearing.
- **Seeding without an installer** — a fresh image's machine catalog is
  derived at build time from the very bundles the image plants
  (`tools/mkimage::library`, T3), so "installer adds a shortcut" holds today
  via the image build + `applib`; when the Stage-8 installer gains a real
  app-install path it calls this same `lib/proglib` write path — no second
  implementation.

Tested host-side in `userland/apps/applib/src/tests.rs` (grammar, every
operation over in-memory seams, every refusal including the denied machine
write, walk bounds, record emission, per-locale Help tokens); help-lint
passes for `en-US` + all required locales. Docs:
`userland/apps/applib/README.md`, `docs/src/userland/applib.md`.

## T3 — Discovery & reconciliation (never a compiled-in list) — **done**

What now guarantees §16.5/§18.5's "no compiled-in app list":

- **The manifest listing (`lib/abi`)** — `AppInfoHeader` carries the
  program-library listing as an explicit **opt-in**: a `library` wire byte
  (`0` = never listed — the default for every command app and service — else
  the `LibraryCategory` folder). An unknown folder byte or a dirty reserved
  field refuses the whole manifest. There is no `show_in_library` boolean and
  no app-class heuristic: a bundle asks to be listed by declaring its folder,
  in its own signed manifest. Alongside it, and **independent of it**, sits
  the optional `library_icon` asset name: the icon is the bundle's own
  identity, drawn wherever the bundle appears (a file-manager tile, a taskbar
  button, a launcher row), so every command app declares one while none of
  them is listed (`plans/ICONS.md`). The manifest TOML source grows the
  matching optional `library` / `library-icon` keys (composer-validated:
  unknown folder, case drift, a `library` on a `service`, or an icon that is
  absent, over-large, or not square master-sized artwork fail the build). The
  C header is regenerated (`cargo xtask c-header --write`); `lib/appload`
  consumers read the listing off the verified header's own accessors.
- **The fold (`lib/proglib::Catalog::reconcile`)** + **`applib rescan`** —
  the walk covers the system program stores then `/Apps` (machine) or the caller's
  own stores (`--user`), breadth-first in sorted order (deterministic
  duplicate resolution), descending nested plain subdirectories but never
  into a sealed `.app`, bounded by `MAX_WALK_DEPTH`/`MAX_WALK_ENTRIES`
  (fail-closed on a tree it cannot believe). Every listed bundle not yet
  catalogued is declared under its manifest folder; unlisted bundles are
  simply not library applications; a malformed/oversized/unreadable
  manifest is skipped and counted, never a scan abort; curation — renames,
  re-files, and hidden suppressions (whose records keep their identifiers
  claimed) — is never disturbed; an unchanged catalog is not rewritten.
- **Default seeding at image build** — `tools/mkimage::library` derives
  `/System/Settings/ProgramLibrary/library.conf` from the planted bundles'
  own manifests and the root-volume author ships it pre-seeded (in the
  writable `/System/Settings` subtree, §16.2), so a fresh install's library
  reflects the shipped graphical apps with **no first-boot rescan** and no
  hand list anywhere (a garbage planted manifest or a duplicate library id
  fails the image build closed).

Tested in `lib/proglib` (reconcile semantics), `userland/apps/applib`
(walk/rescan behaviour and bounds), `tools/mkimage` (derivation, refusals,
and the shipped-store read-back off a built image), and the composer
(manifest acceptance/refusal, wire round-trip, signing).

## T4 — Taskbar leading icon: Program Library — **done**

The bar's leading region is the permanent Program Library launcher.

What now stands:

- `userland/gui/taskbar`: `TaskbarConfig.launcher_extent` sizes the
  leading slot; `BarLayout.library` places it (clipped
  fail-closed to `Rect::EMPTY` on a degenerate screen — never hit);
  `Hit::Library` and the typed `TaskbarResponse::OpenLibrary` route a
  primary press. The button is a `lib/controls` `IconButton`, `Neutral`
  and `PlateSeating::Bar` — quiet peers seated *in* the bar, because on an
  icon strip no single icon is the primary action of the surface. Library
  carries the `lib/icon` `Library` glyph (a three-by-three tile grid) and
  compresses its plate while its popup is open. It wears no role fill or a
  perimeter: it rests as a bare glyph on the bar and washes to `surface_hover`
  under the pointer, with that hover feedback driven through the bar's
  per-surface repaint latch (`Taskbar::take_repaint` → `TaskbarRepaint`).
  The bar owns a copy of the active `Theme` (layout/hit/paint read one
  definition) and the renderer signature dropped its separate theme parameter.
- The file manager is an **autostarted core desktop component**: the session
  spawns it first at bring-up (`spawn_files`) so it takes the **leading
  application-strip slot** (`bar.apps[0]`); it runs windowless at start
  and opens a window on demand; its icon-bar slot menu is built from
  `lib/browse`'s `Places` and has **no Info and no Quit** because it is a
  core desktop app that must not be quit. The bar's **separator** now
  divides the Program Library launcher from the application strip.
- The generic start menu is **gone** (`StartMenu`/`MenuLayout`/`MenuAction`/
  `SessionControl` deleted, §2.14): the session-control rows were wired to
  nothing in the production session (only the taskbar model and tests
  consumed them), so nothing was lost; their real home arrives with the
  Switchboard's System menu (T13). The appearance toggle left the UI with
  the menu — the decision (owner-confirmed) is **no interim seam**: theme
  switching stays programmatic (`DesktopSession::set_theme`) until T13.
- `userland/gui/session`: The file manager is resolved **idempotently** — the
  `LaunchTable` records every desktop-launched child's PID + label + spawn
  path (its attested bundle identity; no app-controlled data), so a press
  on its strip slot raises its served window (`window_of_pid` via the
  window engine's kernel-attested ownership + `DesktopShell::raise_window`),
  lets an in-flight launch finish undisturbed, and spawns only when no copy
  is alive. `OpenLibrary` presents the T5 popup and re-reads the stores.

Tested in the taskbar suite (layout/hit/scale/edges/degenerate +
Library button's pixels), the session suite (routing, raise-vs-launch, launch
table), and the AW3/AW4 QEMU vertical, whose pointer script now clicks the
reveal and then the first app slot.

## T5 — Program Library popup (folder-organised launcher) — **done**

The folder-organised launcher the Library button opens.

What now stands:

- `userland/gui/taskbar::library` — `LibraryPopup`, a pure model over the
  **resolved** `Catalog` the session hands it (`set_catalog`; the popup
  never touches the VFS), composed from `lib/controls`: a `Panel` anchored
  back at the Library button, a `SearchField` filter, one shared `ListRow`
  per folder/entry (folders carry open/closed glyphs and a trailing count;
  entries indent beneath them), and a `ScrollBar` on overflow. Folders
  follow the closed taxonomy order, entries sort by display name, empty
  folders are hidden, and an empty library / empty filter shows a calm
  placeholder ("No programs are catalogued" / "No matching programs").
  Opening is deterministic (search cleared, all folders expanded, cursor and
  scroll at top, keyboard on search). Geometry (`Taskbar::library_layout`)
  opens outward on every edge, clamps along the bar's own span, sizes to the rows
  capped by the available space, and **probes** the shared `Panel` chrome
  rather than re-deriving it; the WM rounds the panel with the same radius
  the chrome draws (§2.2). Controls are static renderers, so reduced motion
  holds by construction.
- **Full keyboard model**: `Tab` cycles search↔rows; arrows wrap, Home/End/
  PageUp/PageDown jump with the view following the cursor; Enter/space
  activates (folder toggles, entry launches); Left/Right fold/climb/descend;
  typing anywhere routes into the search (type-to-filter, case-insensitive);
  Enter in the search launches the first match; Escape clears then
  dismisses. While open the popup holds an **active grab** on the pointer and
  the keyboard at both routing layers (the taskbar router and the session's
  input seat): presses, releases, scroll, and keys all route in wherever the
  pointer is; click-away (any button) dismisses without acting on what it hit;
  nothing at all is delivered to the windows beneath.
- `userland/gui/session` — `library::load_library` reads the machine store +
  the user's overlay through the one `SessionFileReader` seam (the renamed
  graphics-asset seam — one file-read seam, one production impl), parses
  fail-closed, and merges; an absent store is silently empty, an unusable
  one contributes an empty catalog plus a ready-to-print `stderr` warning.
  The `Run` binary loads at bring-up and **re-reads on every popup open**
  (so `applib` edits show live), hands the catalog over with
  `DesktopShell::set_library`, and resolves `LibraryLaunch { entry }` back
  through that catalog to the entry's `Run` path — async-spawned, refusals
  reported loudly by the shared reap path. One present per event, at one
  site, driven purely by the drained per-surface latch: every model change
  latches the surfaces it alters, so a pointer crossing no control presents
  nothing at all.
- **Deliberate deviation, recorded**: the staged text had T5 "offer" a
  right-click *Pin to taskbar* typed action. Pinning is gone from the design
  (T6/T7). An entry row's context menu offers the two things the popup can do
  to a row that its own click cannot: *Open*, and *Create Desktop Shortcut*
  — a symbolic link in the user's own `Desktop` folder, named after the entry
  and pointing at its bundle, which the session creates under its own
  identity (`plans/SYMLINKS.md` S5). Both rows come from the one `EntryRow`
  list, so a reordering cannot re-map what a row does.
  Right-press inside the panel is claimed (modal) and does nothing today.

Tested in the taskbar suite (rows/sort/hide-empty/placeholders, keyboard
nav, filtering, folds, wheel + scrollbar, dark/light/high-contrast pixel
probes), the session suite (loader fail-closed matrix, modality, launch
flow end-to-end, open-popup refresh), and the AW4 QEMU vertical, which now
opens the popup from the planted machine store and launches the terminal
through its catalog entry (keyed by bundle identity, not display text).

## T6 — The application strip: one slot per running application — **done**

The bar's middle is one icon-only slot per *running application*, where an
application is one kernel-attested process. What now stands:

- **The bar** (`tairix-taskbar`) has an application strip between the
  permanent launchers and the trailing group: `TaskbarConfig::app_extent`,
  per-application slots in `BarLayout::apps` (+ `app_strip`), and
  `Hit::App(index)`. The session hands it resolved `AppSlot`s (label, class
  glyph, optional artwork, the windows it owns, its declaration, and its
  manifest-attested `AppIdentity`) through `Taskbar::set_apps`; `AppStrip`
  holds them in the order the session supplies and tracks the hover. Each
  slot paints as the shared `lib/controls` `TaskbarItem` — one visual recipe,
  icon-only: a centred plate-sized icon, never a label, in slots of one
  extent, so a run of applications reads as one strip of equal icons.
- **A slot carries no presence or focus mark.** The presence/accent-seam and
  minimised-plate marks (and the `TaskVisibility` model behind them) are
  **deleted** from `TaskbarItem`: the bar shows which applications are
  running by showing them at all, and a window is reached through the hover
  picker. Only hover, press, focus, and attention states remain.
- **`TaskList` is retained as the one window registry** — id, title,
  minimised — read by the hover picker and by the Switchboard capsule's
  scroll-to-cycle and middle-click previous-window gestures. It is not drawn.
  Its click-to-activate/minimise toggle and its per-window artwork are gone
  with the per-window slots they served; focusing a window restores it, and
  minimising is the title bar's own control.
- **The three left-click cases**, resolved from the declaration alone:
  `AppDefault { app }` when the application declared it handles the click,
  `AppRaise { app }` when it declared none but owns a window, and nothing at
  all when it has neither — the honest answer, never a guessed one.
- **The session** (`tairix-desktop-session`, `apps.rs`) owns the strip:
  `AppBarService` holds every application's declaration as the window engine
  attested it, groups each live served window under the process that owns it
  (the existing launch table + the engine's attested owner records, never a
  window title), keeps a declaring application's slot for the life of its
  process, drops a slot only when it has neither a declaration nor a window,
  bounds the strip at `MAX_BAR_APPS`, and resolves each slot's label, icon,
  and information-panel identity from the **signed** `AppInfo` of the bundle
  the desktop launched that process from — read once per bundle. A process
  the desktop did not launch has no bundle to attest and states no version,
  purpose, or author at all.
- **Per-application icons**: a bundle icon (the manifest's `library_icon`
  asset, SVG or PNG) is untrusted third-party input, so the session never
  decodes it in-process. `lib/image` (complete fail-closed PNG decoder) +
  `lib/compress` `inflate`/`zlib` (RFC 1951/1950 decode) + the `lib/sandbox`
  **image-rendering service** (`imagerender`: SVG via `lib/svg`/`lib/icon`,
  PNG via `lib/image` with alpha-weighted box-filter scaling and aspect-fit
  centring; capped input 256 KiB, side ≤ 512) do the decode in a
  capability-empty worker — the session's own binary re-entered in worker
  mode — and the session verifies, caches (per asset path × side, refusals
  included), and falls back to the shared class glyph on any refusal.
  `Taskbar::app_icon_side` exposes the exact drawn geometry so nothing is
  rescaled at draw time.
- **A window's title still follows its own retitle**: an app that retitles
  over the channel (`WindowRequest::SetTitle`) moves the title bar and the
  registry entry from one call (`TaskBridge::retitle` → `TaskList::retitle`),
  so the two can never name different subjects, and the picker's caption
  follows.

Tested in the taskbar suite (strip span and slot layout on all four edges,
hit-testing, degenerate clipping, a slot's square matching a launcher's at
more than one scale, a stale hover clamped by a fresh strip, the absence of
any presence mark in the painted pixels, per-application artwork with the
class-glyph fallback), the controls suite (the icon-only recipe, artwork,
icon-side probe, an icon drawn with no ink beside it), and the session suite
(the grouping matrix, declaration lifetime, the strip bound, the
manifest-attested identity and its absences, one manifest read per bundle,
and each slot carrying only its own process's declaration).

## T7 — The app→bar contract: declared presence, menu, and picker — **done**

How an application puts itself on the bar and says what its slot offers.
What now stands:

- **The wire**: the app-window channel gained one caller-scoped request,
  evolved in place (`abi-v1` unfrozen): `WindowRequest::SetAppBar(AppBar)`
  (op 13), carrying the event route, whether the application handles the
  slot's primary click, and a bounded `AppMenu`. It is idempotent-replace, so
  an application re-declares to change a row's enablement or its mark. Two
  application-scoped events answer it: `WindowEvent::AppBarDefault` and
  `AppBarMenu { item }`, which is why `WindowEvent::window_id()` is now
  `Option<u64>`. The pin ops (`PinBundle`/`DragOffer`/`DragWithdraw`) and
  `BundleRef` are **deleted**. The declaration is the widest operation, so it
  sets `WindowRequest::MAX_WIRE_LEN` — the endpoint's receive ceiling — while
  each request is framed to its own length, so a declaration's width costs
  the hot Present path nothing (`plans/NEW-MENUS.md` M1a), and the client and
  the session each hold one receive/encode buffer for the life of the
  connection rather than one per call.
- **The menu model** is `AppMenu`: row kinds `Item(AppMenuItem)`,
  `Separator`, `Submenu { label, enabled }`, and `Info`, nested through a
  parent index, bounded by `APP_MENU_MAX_ROWS` (32) rows a plate,
  `APP_MENU_MAX_TOTAL_ROWS` (64) in all, `APP_MENU_MAX_DEPTH` (4) plates of
  chain, and `APP_MENU_TEXT_BYTES` (1536) of row text held in one block
  rather than a widest-case buffer per row. An item states a label
  (`APP_MENU_LABEL_MAX`, 36), an accelerator caption
  (`APP_MENU_SHORTCUT_MAX`, 24), the reason it is disabled
  (`APP_MENU_REASON_MAX`, 64), a mark, and a role — everything the shared
  `MenuItem` control draws. Item ids are non-zero and unique within a menu,
  so an outcome names exactly one row. The wire decoder is held to the
  **same** `check_shape` rule as the builder, so a menu that crossed the wire
  is exactly a menu that could have been built; both are fuzzed in
  `lib/abi/tests/fuzz_decode.rs`. A menu's own title is the application's for
  a per-window menu and is **unrepresentable** on the declaration, whose
  title is the manifest's (`plans/NEW-MENUS.md` M1b).
- **The information panel is manifest-attested.** The application declares
  only that an `Info` row exists; the panel is the session's own `FactList`
  of the bundle's **signed** `AppInfo` — name, version, and the new optional
  `purpose` and `author` fields (`BUNDLE_PURPOSE_MAX = 96`,
  `BUNDLE_AUTHOR_MAX = 64`; `AppInfoHeader::WIRE_LEN` 408 → 568). An
  application therefore cannot state an identity that is not its own inside
  system-drawn chrome, and an omitted field is absent rather than blank.
- **A declaration precedes its declarer's first window.** All five declaring
  applications call `set_app_bar` before they open a window, because a
  declared presence belongs to the process: declared first, the slot carries
  the application's menu and default action from the moment it appears.
  Declared after a window, the session meanwhile derives a slot from that
  window alone — no menu, no default action — so the bar briefly shows a slot
  that answers nothing. The icon-bar QEMU vertical (T15) is what covers the
  ordering: its bar gestures are gated on the session's own witness that the
  first window is on screen, which the declaration now strictly precedes.
- **The engine** (`lib/window`) attests the caller, validates the model on
  decode, and hands it to `WindowHost::app_bar_declared(owner, bar)` — which
  **defaults to refuse** (`Errno::NotSupported`), so a host that composes no
  icon bar fails closed. The route is recorded only once the host accepted,
  dropped in `client_exited` (which also calls `app_bar_withdrawn`), and
  `deliver_app_event` addresses a bar event through *that* recorded route
  rather than anything the event carries. Each delivery path refuses the
  other's events. `WindowClient::set_app_bar` is the client half, and
  `lib/window`'s `appbar::declaration` is the one definition of the desktop's
  **menu convention**: the `Info` row first, the application's own rows next,
  then a rule and *Quit* last. An application supplies only the middle, so it
  cannot place the two ends and cannot get them wrong; `appbar::info_and_quit`
  names the commonest case (the convention's two rows, no default action)
  that `viewer`, `wallpaper`, and `widgets` share, and `terminal` composes its
  *New window* row through the same builder. A submenu row is refused there
  rather than drawn childless — a flat list cannot express parented children,
  so an application needing one builds its own `AppMenu`.
- **The bar states exactly what was declared.** `MenuSubject::App` is the
  menu the desktop's one chain draws for a slot (`plans/NEW-MENUS.md` M3.4):
  every declared row in declaration order with its enablement and mark, a
  declared separator opening the group its next row begins rather than becoming
  a choosable row, a declared submenu's rows on its child plate, and the `Info`
  row's child the information panel. That row is the one whose *label* is the
  desktop's rather than the application's: it draws as `INFO_ROW_LABEL` ("Info",
  with the submenu arrow), so every application reaches its panel by the same
  name. Choosing a row answers `AppMenuChosen { app, item }` and the session
  relays the id straight back — the bar never interprets one. An application
  that declared no menu asks for **nothing**.
- **The hover window picker** (`WindowPicker`) opens at
  `PICKER_MIN_WINDOWS` (two) windows and no fewer, and both its edges are
  timed by the clock rather than by the pointer: it opens once the pointer has
  **rested** on the slot for `PICKER_OPEN_DELAY_NS` (one second), and closes
  `PICKER_CLOSE_GRACE_NS` (200 ms) after the pointer comes to rest on neither
  the slot nor the panel. The grace is load-bearing, not a courtesy: the panel
  hangs a gap away from the bar, so the pointer *must* leave the bar's surfaces
  to reach a cell. Reaching the panel or returning to the slot cancels it.
  Neither edge polls or sleeps — `TaskbarInput::park_deadline_ns` folds the
  pending transition into the desktop's own wait and `TaskbarInput::tick`
  resolves it — and a dwell whose slot moved under it (the strip was
  re-pushed) opens nothing.
- **Cells wrap into a grid, and a grid that overflows scrolls**, so no window
  is ever laid out where it cannot be clicked: `PickerLayout` carries the
  columns, the visible rows, and the shared `ScrollBar`'s gutter; the wheel
  and the bar itself walk the grid; the first visible row is clamped at layout
  time so a density change under a scrolled panel cannot blank it.
- **Thumbnails are prepared a window at a time.** The session answers
  `ShowWindowPicker { app }` with one `PickerEntry` per window, each carrying
  that window's **last presented frame** — pixels the compositor already
  holds — scaled to the cell through `Surface::resampled`, the premultiplied
  entry point to the one resampler (no straight-alpha round trip, no copy of
  the frame). `DesktopShell::advance_window_thumbnails` scales **one** window
  per turn of the serve loop while the dwell runs and
  `window_thumbnails_owed` shortens the park while a slice remains, so a
  picker over a screenful of windows opens already drawn without the desktop
  ever stopping to build it; a slice that lands later fills its cell in place,
  and the pointer leaving drops the prepared pixels. A press on a cell reports
  `WindowChosen { id }`; the picker takes no keyboard and closes when the slot
  is clicked or when the application stops having a choice to offer.
- **`terminal.app` is now one process with many windows.** Each window
  carries its own pty, shell child, screen model, retained picture, look, and
  overlay, over one wait-set with one event mailbox for the process plus a
  shell-output and child member per window. It declares `default_action: true`
  and its own *New window* row through the shared convention
  (`tairix_terminal::appbar` over `appbar::declaration`), so the menu reads
  *Info*, *New window*, a rule, *Quit* — its slot opens a fresh window on a
  click and its menu can close them all. The terminal keeps no window count of
  its own: what a window costs is bounded by the session's per-client frame
  budget and by the process's own stream, process, and address-space limits,
  each derived from the machine and each refusing with a stated reason. Its
  bring-up asks the desktop for the window **before** creating the pty and
  spawning the shell, so a refused window costs one round trip instead of a
  whole process load and teardown. The last window closing ends it.

Tested in the `lib/abi` window-IPC suites (the builder's and the decoder's
shared shape rule, every refusal, the reserved tail, fuzz), the `lib/window`
suites (the refusing default, the route recorded only on acceptance and
dropped at teardown, each delivery path refusing the other's events, and the
shared declaration's rows), the taskbar suite (the declared rows/marks/
disabled rows, the row cap, the relayed ids including from a submenu, the
information panel and its absences, an application with no menu opening
nothing, the three click cases, and the picker end to end), the terminal
suite (the declaration and its row → command mapping), and the session suite
(the window host relaying a declaration and its withdrawal, the picker
becoming its own window, and a cell choice raising the window it names).

## T8 — Notification area upgrade — **done**

The right-side notification area is first-class Reactive Alloy, left of the
reserved Switchboard slot. What now stands:

- **The channel (`lib/abi::notify_ipc`)** — a versioned, fixed-frame,
  fail-closed IPC a producer service posts a transient notification through:
  `NotifyRequest::{Raise { key, severity, title, body }, Clear { key }}`,
  answered with the shared status reply. Title/body are bounded validated
  UTF-8 with no control characters (`NotifyText<MIN, MAX>` — one validator for
  both), severity is the closed `NotifySeverity`
  (Info/Success/Warning/Critical), and every decode refuses a bad magic,
  version, op, severity, over-long/empty title, non-UTF-8/control-char text,
  or dirty reserved tail. It is **not** `#[repr(C)]`/a syscall, so it stays
  out of the generated C header (like `window_ipc`). Fuzzed by
  `tests/fuzz_decode.rs`.
- **The kernel bind** — `NOTIFY_ENDPOINT` joins `WINDOW_ENDPOINT` as the
  seat-scoped reserved set, defined once as
  `tairix_abi::ipc::is_seat_scoped_endpoint` and consumed by the `call_create`
  authority check: an unprivileged desktop session binds it via its live seat
  lease, everyone else fails closed. A squatter cannot claim the rendezvous,
  and losing the seat ends the session (its exit reclaims the endpoint). It is
  unrestricted-sender — a producer's identity is attested per request, not at
  bind.
- **The taskbar (`userland/gui/taskbar`)** — `NotificationArea` holds typed
  `StatusSignal`s (network/volume/battery `StatusKind` → `lib/icon` glyph,
  drawn as calm shared `IconButton`s resolving the loaded `/System/Graphics`
  artwork) and severity-then-recency ordered `TransientNotification`s (`raise`
  upserts by `(producer, key)`, `clear`, `clear_producer`).
  `NotificationsLayout` opens a popover of shared
  `lib/controls::shell::Notification` cards outward from the notification/clock
  region — reusing the library popup's `panel_origin`/`probe_chrome` (§2.2) —
  and fails closed to no cards on a degenerate screen. A status-signal press
  is inert (a live readout, not an action target); a card is click-to-dismiss
  → `TaskbarResponse::DismissNotification`.
- **The session (`userland/gui/session`)** — binds and serves `NOTIFY_ENDPOINT`
  in the desktop run loop, attests each producer via kernel `call_peer_origin`
  (never the wire), relays raise/clear through `DesktopShell::apply_notify`,
  presents/removes the popover window like the other bar popovers, resolves a
  user dismiss, routes a press over the non-modal popover to the taskbar, and
  drops a dead producer's notifications on child-reap.

Status signals carry **no fabricated hardware** — empty by default; their live
tray-signal feed is T9/T10, and the render/`set_status_signals` path is the
complete §27 primitive ahead of that caller. Tested in the abi suite
(round-trip + refusal matrix + fuzz), the kernel suite (the seat-lease bind of
both seat-scoped endpoints, refused without the lease), the taskbar suite
(model ordering/upsert/clear/`clear_producer`, popover layout + degenerate
fail-closed, click-to-dismiss, inert status press, and card render across
dark/light/high-contrast/reduced-motion), and the session suite (the
producer→attest→relay→dismiss path with producer isolation).

## T9 — The Switchboard taskbar icon (always right-most, immovable) — **done**

What now stands:
- `userland/gui/taskbar`: the trailing-most slot is reserved for the
  Switchboard capsule. It is computed **first** among the trailing regions,
  so applications, notifications, and the clock can never displace it (only
  the permanent leading launchers outrank it on a degenerate screen);
  `hit_test` → `Hit::Switchboard`. The mockup microinteractions landed
  (desktop1 panel 6): scroll over the capsule cycles the running tasks
  (wrapping, honest no-op on an empty list), middle-click switches to the
  previous task (an MRU-of-two the task list keeps), hover previews via the
  capsule's instrument readout, and a primary press resolves as a **tap or
  a hold** — a quick release reports
  `TaskbarResponse::OpenSwitchboard { section: CommandSection::Tasks }` (the
  panel's NOW column), a press held past `input::LONG_PRESS_AFTER_NS`
  (500 ms) reports it with `CommandSection::Recovery`, and the readout's one
  safe action, "Open Switchboard", reports the tap's response. One press reports
  exactly one response: the threshold is measured against the monotonic
  time the caller passes to `TaskbarInput::handle`, resolved on the next
  event the router handles (a motion sample taken while the press is held,
  or the release), never by polling or sleeping; a hold that has fired
  never also fires on release; and a press dragged off the capsule opens
  nothing (fail closed).
- The capsule is the `lib/controls::shell` `TraySignal` driven by a compact
  **tray-signal summary** (the T10 feed): its state renders Normal / Job
  Active (badge count) / Resource Pressure / Hung App (danger) / Recovery
  Available exactly as the mockup's icon-states row, using **signal beads**,
  the **pressure rail**, the **heat seam**, **danger state**, and **edge
  wake** from the Reactive Alloy vocabulary. The badge count is the shared
  `lib/controls` count/alert badge (one painter with the Card badge —
  `plans/GUI-CONTROLS-DESIGN.md` §11.27), never a taskbar-private draw. The
  readout's value line previews the busiest task (the summary's top task)
  when the system is calm and the dominant state's own figure otherwise.
- `userland/gui/session`: bind and serve the seat-scoped
  `SWITCHBOARD_ENDPOINT` (the third member of the seat-scoped reserved set),
  accept summaries **only** from the child it spawned (kernel-attested
  `call_peer_origin` pid against the launch table — a foreign publisher is
  refused with a stated stderr line), and feed them to the taskbar. If the
  service is absent or dies, the summary clears and the capsule shows the
  calm Normal state (fail closed, never a crash).
- **Hung App is fed by the session itself, not the service** (owner-approved:
  detection lands now): the desktop's own event deliveries are the one
  honest "not responding" signal — an app-ward window event is a
  non-blocking mailbox send, so an owner whose sends come back refused as
  mailbox-full backpressure continuously for the threshold (4 s,
  `vigil::UNRESPONSIVE_AFTER_NS`) is flagged unresponsive, one accepted
  delivery clears it, and a reap forgets it. The session folds that count
  into the capsule (`HangTracker`, `userland/gui/session/src/vigil.rs`); no
  heartbeat is fabricated and no kernel query pretends to know.

Tested: the reserved slot is trailing-most on every edge and never
displaced (the clock and notifications collapse first on a narrow screen);
the derive matrix maps every summary shape (absent service, calm + top task,
jobs, each pressure kind + count, recovery, hung, and compositions) to the
right state/badge/label/value; scroll cycling wraps both ways and fails
closed on empty; middle-click follows the MRU and fails closed when the
previous task vanished; the tap-or-hold gesture resolves to Tasks, to
Recovery (on the first sample past the threshold and on a still-fingered
release), never twice for one press, and to nothing once the press drags
off; the readout's safe action opens Switchboard while a press elsewhere on
its panel stays inert; pixels prove the capsule, rail, seam, and badge tones
across dark/light/high-contrast; the hang tracker's evidence rules
(backpressure only, threshold, recovery-by-drain, reap, count saturation)
are covered exhaustively; the session suite drives the relay and both
task-switch gestures end to end through `DesktopShell::handle`.

## T10 — The Switchboard component: monitor service + tray-signal feed — **done**

The dedicated, capability-sized process behind the Switchboard (§0).
What now stands:
- New `userland/gui/switchboard` bundle (`kind = "service"`, §16.5) whose
  `AppInfo` requests exactly what this stage uses — `CAP_CONSOLE_WRITE`
  (stderr diagnostics), `CAP_SYSINFO_GLOBAL` (the system-wide process list),
  `CAP_SYSINFO_KERNEL` (the memory-pressure bands) — and nothing ahead of
  its use: `CAP_SYSINFO_HW`, the window channel, and the process-control
  capability arrive with the stages that consume them (T11+, §5.2/CU6). The
  kernel intersects the request with the launching user's ceiling, so an
  ordinary user's Switchboard runs self-scoped (its own processes, overall
  CPU; no kernel memory bands) while an administrator's sees the whole
  machine — authority follows the seat user, never a service account. A
  service bundle never lists in the program library (no `library` key).
- **Lifecycle (owner-approved)**: the desktop session spawns the service at
  bring-up — after binding the endpoint, before the loop — as the logged-in
  user with console inherit, records it in the launch table, and lets the
  ordinary reap path diagnose its exit; the capsule falls back to calm when
  the feed dies. No respawn loop: revival is on demand (T11's open press).
  A copy launched outside a desktop session, or orphaned by a session
  restart, discovers it on its next publish (endpoint unbound → refused
  identity) and exits cleanly with its reason on stderr (§2.24).
- A **sampler** (`lib`-shaped, host-tested over the `lib/procinfo`
  `Transport` seam) that reads the live system on a **tickless one-shot
  cadence** (2 s; sampling is strict on deadline, so an input or command
  wake never re-queries the system; the wait is a single deadline-bounded
  park that also wakes for signals — never a busy-poll, §2.23; the periodic
  sanctioned fallback because system-wide metrics expose no change event):
  the process list (global scope when granted, else self — probed **once**
  at startup so a denied audited query is never spammed), per-process CPU
  deltas keyed on the stable `proc_id` (pid reuse cannot corrupt them),
  overall CPU busy permille (the shared `lib/procinfo::CpuTotals` — one
  arithmetic with `top`/`sysmon`), stopped-process recovery candidates, and
  — when granted — the SMARTRAM memory-pressure band on a slower divider
  (every 5th sample) to bound the audited-query rate. Every refusal
  degrades its field to an honest empty, never a fabrication.
- The **tray-signal summary** (`lib/abi::switchboard_ipc`, versioned,
  fixed-frame, fail-closed, fuzzed): background-job count (**honest zero
  today** — no job registry exists in the OS; the field is real, its first
  producer is later work), recovery count, overall CPU busy permille, the
  dominant measured pressure (CPU by busy-permille hysteresis 900‰ enter /
  800‰ exit; memory by the kernel's own band — never a second policy) plus
  the count of pressured resources, and the busiest task (name + permille)
  for the readout preview. Disk/network pressure joins when a throughput
  query exists to measure it honestly (no such sysinfo query today).
- **Publish on change** over `ipc_call` to the seat-scoped
  `SWITCHBOARD_ENDPOINT`: one publish per changed summary, plus a slow
  keepalive republish (10 s) that doubles as orphan detection; bounded
  consecutive-**fault** tolerance, then a stated exit — never an unbounded
  silent retry (§2.1).
- **Back-pressure is not a fault, and must never cost the service its
  life.** A call endpoint at capacity refuses the post outright rather than
  blocking, so `WouldBlock` says only that the session has not drained its
  queue yet. It is excluded from the give-up budget: the summary stays
  unacknowledged, the change gate re-offers it next sample, and one attempt
  per period is paced by the sampler, not a retry loop. Counting it let a
  busy desktop kill the monitor watching it — and nothing restarts one, so
  the tray capsule stayed dead. The two clean exits (`NotFound`,
  `PermissionDenied`) still catch the genuinely session-less cases, so
  orphan detection is unweakened.
- **The park is re-anchored against the clock it actually parks on**
  (`schedule::park_until`, adopted by `Service::wait_timeout_ns`). `cycle`
  advances the deadline from the reading taken *before* its work, so a cycle
  whose sampling, rebuild, publish and repaint together cost a whole period
  leaves nothing to wait for; parking that remainder would re-enter the full
  cycle at once and keep doing so — a monitor free-running at whatever rate it
  can complete a cycle. `park_until` returns the deadline to hold *and* a
  strictly positive timeout, skipping a wholly missed period rather than
  replaying it, so an expensive cycle costs the work plus a full idle period
  and never more. There is no minimum-wait floor: a floor would only hide the
  overrun as a fast poll.
- **The memory-pressure band is watched, not assumed**
  (`tairix_procinfo::pressure`, permanent wait-set member, armed even with no
  window open). This process caches rendered glyphs, and the process gauge
  admits nothing until it is told the band, so without this every character it
  draws would be an IPC round trip to `fontd`; on a band change it refreshes
  and trims (`plans/FONT-SERVICE.md` §3.2).

Tested: sampler scope probing and degradation, delta/permille arithmetic,
stopped counting, top-task selection across samples, memory cadence divider;
derive matrix incl. hysteresis boundaries and dominance; publisher
change-only + keepalive + failure exits; schedule deadlines (tickless),
including that an overrunning cycle still parks a full period and that the
timeout is never zero at any clock value; the pressure member is armed even
with the window closed;
manifest ∩ ceiling behaviour is covered by the grant-intersection kernel
tests it rides on. Docs: `userland/gui/switchboard/README.md`,
`docs/src/desktop/switchboard.md` (+ the session/taskbar pages), `AGENTS.md`
§3, `PLAN.md`.

## T11 — Switchboard window: the Open Panel (desktop1 panel 2, desktop2a §1–2)

**Deliverables**
- `userland/gui/switchboard` hosts its own screen composition
  (`userland/gui/switchboard::view`, assembled from the shared controls) in a
  real app window (`plans/APPWIN.md` AW2 channel), fed the
  live `SwitchboardModel` from the T10 sampler and emitting `SwitchboardAction`
  the service authorises + applies (pause/resume, switch-to, reveal window,
  quit, force-quit) through capability-checked syscalls; "lower priority"
  stays in T12 with the scheduler surface it needs (§4).
- The T9 gestures now have a target. The **taskbar side stands**: the
  capsule's tap reports
  `TaskbarResponse::OpenSwitchboard { section: CommandSection::Tasks }`, a
  hold past `LONG_PRESS_AFTER_NS` reports `CommandSection::Recovery`, and the
  readout's "Open Switchboard" safe action reports the tap's response. The
  session consumes that response by asking the service to open/raise its window at
  the named section, reviving a dead service on demand — the press is the
  demand.
- The process-control authority lands here whole (§4): the `signal` target
  rule widens in place to "own child, else same-uid, else
  `CAP_PROC_CONTROL`", the capability is minted with this change (holder:
  the Switchboard manifest + the administrative ceiling; enforcement: the
  widened kernel dispatch, audited allow/deny), and the window's task
  actions ride it — an ordinary user acts on their own processes, an
  administrator across principals, every refusal rendered
  `DeniedByAuthority` (§2.24), never fabricated.
- The session→service command mailbox (the reverse direction of the T10
  feed): the service binds an unreserved per-pid report mailbox; the session
  sends open/reveal commands and the seat report (the unresponsive-owner
  set from T9's `HangTracker`, so Recovery lists hung apps with restart —
  via the attested launch path — and force-quit). Every mailbox message is
  authenticated by its kernel-attested per-message `Origin` against the
  session identity learned from the seat-anchored publish reply — never a
  wire claim.
- A resource reading (CPU / MEM / DISK / NET) is drawn with the shared
  `lib/controls` **MetricTile** under its `Track` instrument (a rounded bar
  with the value trace + **pressure rail** colour by resource), never a
  Switchboard-private draw (§2.2). The six sections (Tasks, Jobs, Pressure,
  Activities, Recovery, System) render the NOW / BACKGROUND / PRESSURE /
  RECOVERY / SYSTEM content from live data.
- Zero-copy window presentation, parks on its event mailbox (no poll, §2.23),
  fail-closed on a denied action (renders `DeniedByAuthority`, never acts).

**Tests**: model→controls rendering from a fixture; each action emits + is
authorised/denied correctly; meters render each pressure level; scroll/keyboard
per section; both themes + high-contrast + reduced-motion.

**Done when**: the Open Panel matches the mockup, driven by live data, actions
authorised server-side; gate green.

**Status — the Switchboard service side (`userland/gui/switchboard`): done.**
The service binds its per-pid command mailbox, learns the session identity
from the publish reply, and authenticates every command against that
message's kernel-attested `Origin`. One `waitset_wait` covers the next
real sample deadline, the command mailbox and — only while open — the
window's event mailbox. `OpenPanel` opens the composition on the mapped
`Switchboard::select_section`; a second one raises (`ActivateOwner` naming
its own pid) and switches section; close returns to headless sampling, which
never stopped. Each new sample is shown through `Switchboard::set_model`, so
a live refresh keeps the section, every section's scroll offset, and the
keyboard focus the user set. Live model: tasks and recovery from the sampled
process list, the seat report's owner ids joined against those sampled
names, CPU/memory `ResourceSummary` meters carrying the measured value, a
bounded rolling CPU history plotted as the band's `Chart` and the pressure the
T10 derivation latched.
Actions: task→`ActivateOwner`, restart→`RestartOwner`, force→`signal(Kill)`
gated on `CAP_PROC_CONTROL` read through `cap_query`, close→destroy; an absent
authority renders refused and is never attempted, a task id beyond the
syscall's signed width is refused not truncated, and every refusal is stated
on `stderr` without ending the service. Manifest adds `CAP_SHM` and
`CAP_PROC_CONTROL`.

`jobs`, `services`, and `system_actions` are **empty by necessity, not
omission**: no background-job registry exists, the System Information API has
no service-enumeration query, and no power/lock interface exists for this
service to drive. Disk and network resource rows are absent for the same
reason — no throughput query. Filling them needs those interfaces to exist
first.

**Status — the desktop-session side (`userland/gui/session`): done.** A
successful publish is answered with `encode_publish_reply` carrying the
session's own kernel-attested `ProcId` (`tairix_rt::self_origin`, read once
at bring-up), so the service can authenticate the commands the session
sends; a refusal stays the plain status frame. `ActivateOwner` is validated
against the live window registry (the owner must hold a served window on
this seat now) and raises through the session's one focus/raise path;
`RestartOwner` resolves the owner through the `LaunchTable` and re-enters
the one attested spawn-and-record path; either naming an owner the session
cannot act on is `Errno::NotFound`, stated on `stderr`, model untouched.
The capsule's `TaskbarResponse::OpenSwitchboard { section }` is relayed as
`OpenPanel` on `command_endpoint_for(<service pid>)` as a non-blocking
send; with no instance live the press revives the service and holds *one*
pending open, delivered on that instance's first publish and cleared. The
seat report is sent only when the vigil's unresponsive set changes,
carrying the truthful total beyond `SEAT_REPORT_OWNERS_MAX`. A refused send
is reported on `stderr` and dropped, never retried.

**Status — the shared controls (`lib/controls`): done.** `metric.rs`'s
`MetricTile` under `MetricInstrument::Track` is the one reading-with-a-track:
one resource reading (label, reading text, rounded track
tinted by the resource's semantic rail through the same `signal_color` lookup
`Card`'s Pressure Rail uses), read-only with no input or action. `chart.rs`
adds its history counterpart, `Chart` (spec §11.35): a bounded
oldest-to-newest permille series plotted as a line with a quiet filled body,
tinted by the same rail lookup, mapping its readings across the *whole* box it
is given — a trend confined to a track's thickness cannot rise more than a
pixel or two whatever it reads.
`MeterValue::Unmeasured` makes an unmeasurable resource
unrepresentable as a real zero, so a denied or absent query draws a quiet
groove instead of a fabricated `0%`. The `switchboard` composition draws a
reading in the section it is *about* — there is no permanent resource band
above the sections (`plans/NEW-SWITCHBOARD.md` S2) — and a column shows one
instrument: the `Chart` takes the slot the tile's track would have had
wherever there is a history to plot, so a resource is never reported twice in
the same column. The System report
carries each measured value, its pressure and its inline bounded history
once, feeding the System section's own resource tiles. `select_section` lets
a host open on a chosen section and `set_model` refreshes live data in place —
both through the one internal transition, so the location band, content and
per-section scroll can never disagree; a refresh keeps section, offsets,
focus, pointer and any in-flight drag, and deliberately drops row-indexed
selection, hover and any armed press so a press begun on one row can never
complete against its replacement.

**Status — the taskbar side (`userland/gui/taskbar`): done.** The capsule's
primary press resolves as a **tap or a hold** into
`TaskbarResponse::OpenSwitchboard { section }` — tap → `CommandSection::Tasks`,
hold past `LONG_PRESS_AFTER_NS` (500 ms) → `CommandSection::Recovery` — and
the readout's new "Open Switchboard" safe action reports the tap through that
same one route. The hold is resolved from the `now_ns` the embedder passes in (the
next motion sample or the release), never a spin or sleep; a fired hold never
also fires on release, and a press dragged off the capsule is cancelled
outright rather than re-armed. T10's interim pin-on-press API
(`set_pinned`/`is_pinned`/`toggle_pinned`/`release_tray_pin`) is deleted, not
aliased.

**Status — the kernel authority: done.** `signal`'s target rule is widened in
place to **own child → same principal → `CAP_PROC_CONTROL`** (id 40, granted
in the administrative ceiling only, so an ordinary user's panel renders the
force control refused). `ProcessSignal` is split into `resolve_child` (the
unchanged own-child lookup) and `signal_task` (delivery to an already
authorised target) over one shared delivery engine the console foreground
path reuses; the combined method is deleted. The decision lives in the
syscall handler beside every other capability check: the narrowest rule first
so job control keeps working on the standing grant of having spawned the
child, a non-positive pid refused before any table is consulted, the target's
owner read from the kernel's own capability record, and every cross-principal
outcome audited once (event **4036**, `Warn` on refusal, carrying caller,
pid, target, signal and the deciding rule as one value so the record and the
verdict cannot diverge).

## T12 — Pressure view + Activities grouping (desktop1 panels 3–4, desktop2a §2–3) — **done**

The panel carries six sections — Tasks, Jobs, **Pressure**, **Activities**,
Recovery, System — on the same in-place `SwitchboardModel` (no v2), with
the wire `CommandSection` — which the bar's own gesture names directly, so the
section a user asked for is relayed unchanged rather than re-decided — and the
service mapping extended in step.

**Pressure** ("why is my machine slow") shows one cause `Card` per resource
the **tray's own latches** flag (CPU ≥ 900‰ enter / < 800‰ exit; memory
band ≥ mild) — measured, never guessed: the card names the sampled culprit
(busiest task by CPU delta; largest `mem_bytes` for memory), renders the
pressure rail by kind and the heat seam from the measured rate
(`ActivityState::Progress`), and offers only actions that genuinely work
today: **Pause** (`Stop`), **Lower priority** (the new syscall below), and
**Show tasks** (widget-internal jump to the culprit's row). Each action
carries a truthful verdict — `Ready`, `DisabledByState` (already `Low`,
already stopped), `DeniedByAuthority` (rendered from the same owner-uid +
`CAP_PROC_CONTROL` rule the kernel enforces, re-checked at apply) — and the
boards' "Sleep app" / "Throttle" stay absent: no app-nap or disk-throttle
interface exists, and no disk/network latch exists to hang a card on
(`docs/src/desktop/switchboard.md` records the divergences).

**Activities** are **live, session-lifetime groupings of running
processes**, held by the monitor service keyed on the never-reused
`proc_id` (`userland/gui/switchboard/src/activities.rs`): single
membership, auto-named "Activity N", inline rename (trimmed, unique,
≤ 48 chars, refusals stated), bounds 12 groups × 32 members rendered as
disable reasons, members pruned — and emptied groups dissolved — only on a
sample whose process list succeeded, so degradation never wipes groupings.
Set actions: **Switch** (`ActivateOwner` per joined member, reverse order so
the first lands frontmost), **Pause/Resume** (`Stop`/`Continue` sweep),
**Close** (`Terminate` sweep — graceful; force-kill stays Recovery's — then
dissolve), all sweeping **only members joined to the current sample** (a
stored pid may have been reused; unjoined members are skipped by
construction). Tasks rows gained a `Group` button opening a `Menu` popup
(assign / new activity / remove, with honest disabled reasons); grouping
edits mark the panel dirty and are presented once in the same wake, before
the service parks again. The
mockups' Snapshot/Hibernate are absent: no process snapshot interface
exists. Keyboard completeness came with it: a horizontal action focus
(Left/Right + Enter) reaches every row button in every section — fixing the
pre-existing gap where Recovery's Force was keyboard-unreachable.

**"Lower priority" scheduler surface** (the plan's promised new surface):
`SchedulerPolicy::set_priority`/`priority` — recorded at once, governs the
next enqueue, idempotent, fail-closed on unknown/terminal ids — implemented
by all three policies (CFQ/EEVDF re-derive their 4:2:1 weight per enqueue;
MLFQ re-bands with fresh yield residency, its demotion/boost dynamics — and
the starvation guarantee — untouched) and pinned policy-neutrally by the
shared conformance suite. Syscall **104 `sched_set_priority(pid,
SchedPriority)`** (`SchedPriority{High=1,Normal=2,Low=3}`, 0 reserved,
fail-closed decode): target rule shared with `signal` through one handler
helper (own child → same principal → `CAP_PROC_CONTROL`), plus a **raise
gate** — any change toward `High` needs `CAP_PROC_CONTROL` whatever the
target, so no user outweighs other principals' fair share. Audited per call
by the dispatcher plus one `PROCESS_PRIORITY_CHANGE` decision record (id
**4037**, `Info`/`Warn`, caller/pid/target/priority/rule/raise; own-child
lowering stays unrecorded like own-child signals). The sysinfo
`ProcessRecord` carries the live `priority` (the old reserved byte 59) read
from the scheduler's record, which is what lets the panel render an
already-lowered culprit's action spent; `tairix_rt::sched_set_priority`,
the `tairix_sys_sched_set_priority` C stub, and the regenerated headers
(`TAIRIX_SCHED_PRIORITY_*`) expose it, and the syscall/dispatch/fuzz/
proptest oracles cover it end to end.

## T13 — System menu / quick actions + System Settings access (desktop1 panel 5) — done

Where System Settings lives (issue requirement: **not** in the library).

The quick-actions menu opens on a **secondary press on the Switchboard
capsule** (desktop1 panel 5) and is drawn by the seat's one menu chain like
every other menu on the desktop (`plans/NEW-MENUS.md` M3.4). The taskbar holds
no authority: each row reports a typed outcome and the session resolves it.

**Rows, in order** (`—` marks a group divider):

| Row | Resolved by | Backing |
|---|---|---|
| About This System | session → Switchboard `System` | the System screen (identity, uptime, load, memory) |
| System Monitor | session → Switchboard `Tasks` | the T11 open panel |
| Task Shell | session → launch `os.tairix.terminal` | the graphical terminal bundle |
| — | | |
| Light / Dark Appearance | session `ThemeRegistry::set_theme` | §10; the active one is the group's chosen member — a bullet, disabled, with its reason |
| — | | |
| Lock Screen | session `ScreenLock` → `ElevateRequest::Verify` | the per-console elevation broker |
| Log Out | session exits cleanly | the login supervisor re-prompts |
| Restart | session → Switchboard → `system_power(Restart)` | `CAP_SYSTEM_POWER` |
| Shut Down | session → Switchboard → `system_power(PowerOff)` | `CAP_SYSTEM_POWER` |

**Design decisions**

- **Grouping is a row property, not a row.** `lib/controls`'s `MenuItem`
  gained `with_group_break(true)`, which draws a divider rule in the gap
  *above* that row. Modelling a divider as a pseudo-row would have made
  "a separator cannot be focused, hit-tested, or activated" a runtime guard;
  as a property it holds by construction, keyboard navigation needs no
  skipping, and every index the control reports stays a direct index into the
  owner's own command list. A point inside a divider band belongs to no row.
- **Destructive rows confirm.** Restart and Shut Down carry
  `ControlRole::Destructive` (leading danger rail) and act only through a
  modal `Dialog` whose safe choice holds the default focus and where Escape
  cancels — a single click never ends the machine.
- **Launch rows resolve through the catalog, never a compiled-in path.** A row
  whose bundle is absent from the program-library catalog is disabled with a
  stated reason rather than emitting a launch that would fail.
- **Lock heads the last group.** It is the one way out of the session that
  *keeps* the session; everything below it ends work in progress.
- **A lock that could not be undone is never offered.** `SystemPermits`
  carries `lock_available`, which the bar fills from the one console
  attestation `set_elevation_available` — the session's single
  `elevate_endpoint(self_origin().console())` read, since "can this session
  re-authenticate somebody" is also exactly what the clock menu's set-time
  row needs (T17); two booleans holding the same value by definition would be
  the duplication §2.2 forbids. It defaults to refusing, so a bar that was
  never told renders the row non-actionable with the Authority Mark and a
  stated reason rather than stranding the user behind a prompt nothing can
  answer.

**Rows deliberately not shipped, and what each waits on.** A row that cannot
act must not exist, so these are absent rather than present-but-dead:

- **New command** — the session's spawn path passes no argv and the terminal
  bundle accepts no command to run, so there is nothing for the row to
  invoke. It needs an argv-carrying launch path (`plans/APPS.md`,
  `plans/PTY.md`), not a menu entry. The clock's set-time row (T17) is
  unaffected: the program it starts is interactive and collects its own
  input, so the elevated launch carries no argv either.
- **Services** — the System Information API has no service-enumeration query,
  so the Switchboard's `services` list is honestly empty. It lands with the
  service manager (`plans/NEW-SERVICEMANAGER.md`), which owns that query.
- **Permissions** — there is no graphical capability-inspection surface, and
  `cap_query` answers only about the caller itself. It needs a real
  permissions view before it can have a menu row.
- **Configure** — `configure` is a console command app; with no argv-passing
  launch path the desktop cannot run it in a window, and a settings *surface*
  is separate work. System Settings therefore remains reached from the shell
  until that surface exists; it is still **not** a program-library folder.

**The lock surface** (`userland/gui/session/src/lock.rs`, `ScreenLock`).
Locking is the one way out of a session that keeps the session: everything
behind the lock carries on running, but nothing on screen is legible and no
event reaches it. Three properties, each load-bearing:

- **It covers the screen.** An opaque surface at the compositor's full
  extent, so a passer-by learns nothing about what is on the machine.
- **It takes every event.** While `is_locked()`, the session drains the seat's
  pointer and keyboard *straight into the lock* rather than through
  `DesktopShell::pump`/`handle`, so no motion, click, or keystroke reaches
  the window manager, the taskbar, a served application, or the confirmation
  prompt. On a mid-batch unlock the remainder of the batch is drained and
  **discarded** — it is the tail of the password-entry gesture and must never
  land in the session that just became visible.
- **It stays on top.** `keep_topmost` raises it before every composite, so a
  window opened or raised behind the lock cannot surface over it.

Authentication is not the lock's to decide: it offers the password to the
per-console broker and believes only `Verified`. A refusal, a transport
failure, an absent broker, and an unparseable reply are one answer — still
locked. It deliberately holds no attempt counter or rate limit; the broker
owns that policy and audits every attempt, and a second copy here would be a
second place to get it wrong.

The password lives in exactly one place, the masked field's own bounded,
pre-reserved buffer, and is erased on every path out — verified, refused,
unreachable, or abandoned at teardown.

**The masked field** (`lib/controls`, `TextField::secret(max_len)`) is a mode
of the one shared text control, never a second text entry. It draws one
filled bead per character rather than a repeated glyph, so the rendered run's
width depends only on the length and no particular glyph need exist in the
font; hit-testing maps x onto fixed bead cells and always lands on a char
boundary. Secret mode is bounded so the buffer is reserved once and typing
can never reallocate and strand a copy of the secret in a freed block;
replacing, clearing, and dropping erase it, and `Debug` redacts it. The erase
itself is the workspace's single `tairix_util::secret::wipe` — a volatile
write an optimiser cannot delete as a dead store, now shared by the login
prompt, the broker, the shell's `elevate` builtin, and the runtime's
elevation client.

**Tests**: the row table states the expected labels, groups and roles, with the
appearance in force marked as its group's chosen member, for both appearances; a
secondary press on the Switchboard capsule asks for the menu and a press
elsewhere does not; an unpermitted power row is non-actionable, carries the
Authority Mark and states its reason; power rows are denied when no authority
has been published; the lock row is denied until the session attests its console
has a broker; a launch row with no catalog entry is disabled with its reason;
each row maps to exactly the expected typed outcome and a row left out shifts no
other row's; the plate, the grab and the one answer are the chain's
(`plans/NEW-MENUS.md`); the confirmation dialog
relays exactly once on confirm and nothing on cancel or Escape; the power
relay round-trips and rejects malformed input; the Switchboard acts only when
it holds the capability. For the lock: engaging covers the screen and is
idempotent; a wrong password, an unreachable broker, Escape, Enter on an
empty field, and a pointer press all leave it locked; a correct password
unlocks and removes the surface; the verifier is offered exactly what was
typed and never a retained previous attempt; `keep_topmost` raises over a
later window; `abandon` and `repaint` behave. For the masked field: one bead
per character, a render independent of which characters are held, no glyphs
drawn, caret and selection on cell boundaries, the bound enforced, the buffer
never reallocated, and erase-on-replace/drop with a redacting `Debug`.

**Done when**: the quick-actions menu exposes the session/power controls and
the Switchboard/appearance surfaces above, every shipped row acts for real,
and the gate is green.

## T14 — Reactive Alloy fidelity pass — done

The full design vocabulary is present as shared `lib/controls` behaviour and
used by the taskbar and the Switchboard; no surface draws its own.

**Vocabulary → where it appears** (all from `lib/controls`, §2.2):

| Reactive Alloy element | Surfaces |
|---|---|
| **Pressure rail** | Switchboard resource meters + pressure cards; the Switchboard tray icon under pressure |
| **Live seam** | task rows with live CPU/IO activity |
| **Signal badge / bead** | tray icon job/alert/recovery counts; notification & recovery items |
| **Edge wake** | the Switchboard's action column, while its list is displaced |
| **Danger state** | hung-app icon, force-quit/power actions, destructive dialogs |
| **Heat seam** | background-job progress; pressure live-rate |
| **Focus field** | the focused row and every one of its actions, as one group |
| **Action beads** | summary counts on action groups |
| **Magnetic motion** | `lib/theme`'s timing model; controls stay aligned as the system moves (reduced motion collapses each transition to an immediate state change) |

**Focus Field.** `FocusState` holds two orthogonal facts — holds the keyboard,
and belongs to a highlighted group — and the shared `resolve_frame` recipe
turns membership into a partial lift of the member's rim toward the active
rim, so every family inherits it from one definition. `apply_focus_marks`
sets membership from the same `focus_here` fact that places the ring, so the
two can never disagree: the focused row (or card), all of its action buttons,
and nothing else are the group. A control that is both focused and a member
takes the ring only — the language draws one or the other, never both. A
filled plate keeps its matching rim (tinting it would put a foreign edge on a
coloured control), and heavy contrast lifts all the way to the active rim
rather than relying on a blend a high-contrast palette would flatten.

Membership is the weakest claim a rim can carry, and a rim-owning disposition
outranks it: a disabled, denied, needs-capability, failed-closed, or pending
control draws identically in or out of a highlighted group. Each of those is
stating something the user needs far more than which row a control belongs
to, and a control that cannot be actioned must never look livelier than a
resting one that can. Only an ordinary interactive control is lifted —
including one awaiting confirmation, which is still actionable and still
takes its plain role emphasis.

**Edge Wake.** An anchored control does not move while content scrolls past
it, so a still frame cannot say whether it is pinned or merely where the rows
left it; the wake answers that on its edge. `paint_edge_wake` lights the
leading edge of the Switchboard's action column for exactly as long as the
list beside it is displaced, at the shared seam breadth, doubled under heavy
contrast. It is a *state*, not an animation: nothing fades, so reduced motion
needs no second path and a screendump carries the same information as a live
surface. A card section has no wake — a card draws its own footer actions
inside itself, so no anchored column stands beside the list. The column's
geometry comes from the same `split_row` the buttons are laid out with, and
the per-section action count is now the single `row_actions(Section)` the
render pass, the hit-test pass, and the Group popup's anchor all share (it was
a bare literal restated at nine sites, so a click could have landed on a
button the user was not looking at).

**Tests**: the rim lift is visible, partial, absent on a filled plate, full
under heavy contrast, and present under both appearances; focus beats
membership on one control; every rim-owning disposition draws identically in
or out of a field while one awaiting confirmation is still lifted; the
focused row's whole action group is marked and no other row is; leaving the
content region clears the field; the field is visible in the rendered pixels.
For the wake: absent unscrolled, present when scrolled, cleared on scrolling
back, absent for card sections, thicker under heavy contrast, and landing
exactly on the first action button's left edge.

## T15 — Documentation, integration tests, and the validation gate

**Documentation: done.** `docs/src/desktop/taskbar.md` (icon-bar layout,
program library, application strip, declared menu, hover picker, notification
area), `docs/src/desktop/switchboard.md`
(the system-overview surface, feed, sections, actions, capabilities, and the
relayed power transition), `docs/src/desktop/session.md` (the icon-bar service,
every quick-action outcome, and the trusted power-confirmation prompt),
`docs/src/desktop/theming.md` (the live Light/Dark control), and the
`docs/src/lib/` pages for `proglib`/`controls`/`image` are current
against the built surfaces, as is the crate `README.md` beside the monitor
service. The `README.md` support matrix needs no row: the desktop is
architecture-neutral, and the security matrix's re-authenticated screen-lock
row already covers T13.

**Prerequisite settled while landing this stage.** Homes were being created
bare — no `Settings/`, no `Desktop/`, none of the fixed shape — so the very
first per-user write of any kind failed `NotFound` on a real install. The
home's shape is now one shared definition (`tairix_users::{HOME_MODE,
HOME_SUBDIRS}`, `plans/USERS.md` U1) that the `CAP_USER_ADMIN` provisioning
path, `tools/mkimage`, and the QEMU users-root fixture all read.

**QEMU vertical: done.** `tests/integration/appbar_qemu_aarch64` is a
dedicated short vertical rather than a fourth stage on the already long
`autoload_input_qemu_aarch64` choreography, so a gate mis-count in one
cannot wedge the other (D20). It boots the same graphical world (the
`AutoloadRootDisk` image, unlock → login → `desktop`), then opens the
program library, launches the terminal from its row, right-clicks the slot
the session gave that process on the bar, chooses *New window* from the menu
the **application itself** declared, and finally primary-clicks that same
slot to take the default action the declaration claimed.

What holds it together:
- **Every coordinate is the product's own.** The script drives a host
  `DesktopShell` with the very events the guest will receive — through
  `TaskbarInput`, over a `Catalog` rebuilt from the same manifests the image
  plants (`reconstructed_library`) — so the menu it clicks is the menu the
  model opened, built from the terminal's own `appbar::declaration`, and its
  row comes from `Menu::row_rect` at the row the declaration named rather
  than at a counted position. Nothing is a hand-copied pixel.
- **Two PASS witnesses, each attributable act by act**: an `APP_LOADED`
  naming the terminal's bundle, and **three** window creates on the reserved
  endpoint — recognised by the wire length unique to a create among that
  endpoint's replies. The desktop's own surfaces are session-painted
  compositor windows that never call the window channel, so the launched
  application is the only client that opens one: the three are its launch
  window, the chosen *New window* row, and the slot's declared default
  action. That attests the whole contract — the declaration was accepted, the
  slot was the declaring process's, and both outcomes were delivered to it.
- **Each side gates on what it can honestly state.** The guest's audit sink
  sees kernel audit records, so it counts creates. The host reads the serial
  transcript, so it gates on the session's own per-window `WINDOW_SHOWN`
  announcement — the witness that a frame carrying that window's first
  painted pixels reached the display. Neither infers the other's facts:
  on this endpoint a present, a blur change, a retitle and a declaration all
  answer with the same four-byte status reply, so "the reply after the create
  is the first present" is a guess about how many requests an application
  makes, and on a shared rendezvous a guess about the other clients too.
- **Every gesture is gated causally, not by a timer.** The bar gestures wait
  on `WINDOW_SHOWN`: by then the application has declared its bar (it
  declares before opening a window), the session has grouped that window
  under its attested owner, and the strip has been re-resolved and drawn — so
  the slot is live in the bar the guest hit-tests, under any host load. A
  create reply would say only that the window exists, which is too early both
  to photograph and to click.
- **The guest cannot exit out from under the evidence.** The last window is
  opened by the last gesture, and the runner sends no pointer step until
  every dump already asked for has been read back and parsed — so the create
  that completes the PASS cannot happen until the final dump is on disk.
- **Three dumps read the screen.** The bar before anything runs (the
  baseline the others are read against, and the frame that proves the
  composited wallpaper); the first application slot carrying that
  application's glyph with its window over the first cascade slot; and both
  windows up with the one application still holding exactly one slot — the
  frame that proves the bar shows applications rather than windows. That last
  claim is asserted rather than merely described: the slot *beside* the
  running application must be pixel-identical to the bare frame, so a
  regression giving each window its own slot fails here instead of satisfying
  assertions that all read the first slot. A window is probed at its cascade
  slot rather than by its whole rectangle, because the terminal's window is
  whatever its character grid measures in the face the running font service
  resolved, which no host reconstruction can know.

**Defect found and fixed by this vertical.** The capsule press relays
`OpenPanel` to the pid the launch table names, but a process exists from
the moment it is *spawned* while it binds its command mailbox only once its
bundle has loaded — whole seconds on a cold boot. A press in that gap was
sent to a mailbox that did not exist and vanished, so the capsule silently
did nothing; the diagnostic even blamed a full mailbox for what was a
missing one. `SwitchboardMailbox::send` now answers whether the instance
took the command: `open_tray` holds a refused gesture as the single pending
open (the instance's own first publish carries it through — no retry loop),
`deliver_pending_open` puts a back-pressured one back rather than dropping
it, `relay_power` reports a refused confirmation instead of letting a
confirmed shutdown pass for success, and the production seam names the
kernel's actual reason.

**Gate**: `cargo fmt --all`, `cargo xtask ci` (once), `cargo xtask fuzz
--secs 5`, and `tools/ci/soak.sh both --secs 20`; any failure is fixed in the
same change.

**Done when**: docs current, the integration vertical green, and the
whole-project gate green — all three met.

## T16 — The desktop icon surface — **done**

The user's own `Desktop` folder, shown as icons on the desktop itself. What
now stands:

- **A desktop layer in the compositor** (`userland/gui/wm`): an optional
  `desktop: Option<Surface>` composited between the background fill and
  every window (`set_desktop` / `clear_desktop` / `desktop_bounds`), damaged
  exactly over what it covered and encoded as its own layer beneath the
  windows on the accelerated path. It carries **no window id**, so it can
  never be raised, focused, closed, or reached through the ordinary window
  z-order — it is the floor, not the bottom window. Input belonging to no
  window arrives as `InputResponse::DesktopPointerMoved` and
  `InputResponse::DesktopKey { key, modifiers, pressed }`;
  `DesktopPointerMoved` carries **no position**, because the router already
  holds the one authoritative pointer (`InputRouter::pointer`) and a second
  copy on the wire could disagree with it.
- **One icon grid, two flows** (`lib/browse::layout`): `GridView` is
  parameterised by `GridFlow` — `RowsFromLeading` for the file manager's
  row-major scrolling grid, `ColumnsFromTrailing` for the desktop's
  trailing-edge column that grows a new column inward as it fills — so both
  share one cell geometry, one hit-test, and one set of counts
  (`cells_per_line`, `lines_total`, `visible_lines`, `visible_range`). The tile
  is shared too: `grid_tile`, `entry_label`, and `grid_metrics` are public, so
  the desktop paints the *same* `lib/controls::IconTile` — the plateless
  picture-over-name item — as the file manager rather than a lookalike; there is
  no second icon-tile painter. Neither view ever lays out a tile an edge would
  cut short; the one parameter they deliberately differ in is `GridFill`: the
  desktop's field takes `FixedPitch`, keeping its icons anchored to the edge they
  hug whatever the work area's exact extent is, while the file manager's
  resizable grid takes `Spread` and shares a row's leftover width out between
  its tiles.
- **The surface** (`userland/gui/session::desktop`): `Desktop<S:
  DirectorySource>` lists the user's `Desktop` folder through the same
  directory seam the trusted file picker uses, sorts and classifies it with
  the shared engine, and paints through the shell's icon-artwork lookup —
  shipped folder artwork for a folder, content-class artwork for a file, and
  the built-in glyph when an asset is absent or refused, so a tile can never
  blank. Hover, press to select, press on empty desktop to clear, the shared
  `DoubleClickTracker` to activate, and — while the desktop holds the
  keyboard — arrows to move (down/up one icon, left/right one whole column),
  `Enter` to activate, `Escape` to clear. A folder that will not list shows
  nothing rather than failing.
- **Activation resolves by kind, and refuses loudly**: a directory opens the
  file manager *at that path* (its first argument, which the files app now
  honours); an application bundle launches its `Run` binary; a plain file
  resolves its association through the catalog the session holds and
  launches that application with the file as its argument; and a file
  nothing is associated with is refused with the reason on `stderr`, the
  icon left selected. Every launch rides the session's existing asynchronous
  path (`plans/FIX-DESKTOP.md`), so the compositor never blocks on one.
- **Re-listing is gesture-driven and rate-limited, never timed.** The system
  has no filesystem-change notification, so the desktop re-lists at
  bring-up, when the session asks (a forced re-list ignores the limit), and
  on pointer arrival — no more often than `RELIST_MIN_INTERVAL_NS`, so
  sweeping the pointer on and off cannot turn a gesture into a stream of
  directory reads. There is deliberately **no timer and no polling loop**: a
  periodically-waking desktop would keep a core busy to discover nothing. A
  re-list keeps the selection on the same named icon, and one that changed
  the folder also refreshes the library catalog and the file associations
  (`DesktopOutcome::relisted`), so an application installed after bring-up
  is picked up without a restart.

Tested in the wm suite (the layer draws over the background and under every
window, a layer smaller than the screen leaves the background showing,
setting and clearing it damages exactly what it covered, the accelerated
scene carries it beneath the windows, and the two desktop input responses)
and in `userland/gui/session/src/desktop_tests.rs` (shared sort order, an
unlistable folder, selection kept or dropped across a re-list, the rate
limit and the forced re-list, hover and its clearing, press-to-select and
clear-on-empty, the keyboard model with its clamps and its silence when
unfocused, every activation branch including the unassociated-file refusal
and the too-slow second click, and painting — every shown icon draws even
with no artwork store at all, an empty folder leaves the layer fully
transparent).

Docs: `userland/gui/wm/README.md`, `userland/gui/session/README.md`,
`docs/src/desktop/wm.md`, `docs/src/desktop/session.md`,
`docs/src/desktop/apps.md`.

## T17 — The clock's menu: setting the machine's date and time — **done**

The clock was inert: pressing it reported a typed `ClockPressed` outcome the
session listed among the responses it deliberately did nothing with. It now
carries a menu, and that outcome is gone (§2.14) — the press opens the menu
instead, on **either** button, since a menu is the clock's only behaviour and
being particular about which button asks for it would only surprise.

**Rows, in order** (`ClockRow`, one table both the render and the row →
command mapping derive from, so a row cannot exist without a command behind
it or the reverse):

| Row | Resolved by | Backing |
|---|---|---|
| the reading the bar is drawing | nothing — a statement | the bar's own clock label |
| Set Date & Time… | session → `ElevateRequest::Launch` | the per-console elevation broker, then `datetime.app` under `CAP_TIME_SET` |

**Design decisions**

- **The heading states the bar's own label, never a second reading.** The
  session owns the wall clock and hands the bar the spelled text; the menu
  repeats that string. A menu that re-derived the time could disagree with
  the bar beside it. An unset clock — nothing has established a wall time this
  boot, so the bar draws the `clock::UNSET_LABEL` placeholder rather than
  vanishing — has its heading say *Time not set*, never a repeat of the
  dashes and never a fabricated `00:00`.
- **One console attestation serves two rows.** Whether the *Lock Screen* row
  (T13) can act and whether the set-time row can act are the **same** fact —
  does this session's console have a re-authentication broker — so the
  taskbar carries one `set_elevation_available` attestation, set from the one
  `elevate_endpoint(console)` read the session already made. A second boolean
  holding the same value by definition would be the duplication §2.2 forbids.
  It defaults to refusing: a bar that was never told offers neither.
- **A missing broker is rendered refused, not hidden.** The row draws with
  the Authority Mark and its reason stated, and emits nothing while denied —
  the same shape as an unpermitted power row, so the user learns *why*
  instead of hunting for a row that is not there.
- **The desktop never sets the clock, and never gains the capability.**
  `CAP_TIME_SET` is not in a session's manifest and must not be. The session
  asks for an account that holds it through its own trusted credential prompt
  (`userland/gui/session/src/elevate.rs` — a session-owned window, so a
  password is typed into desktop chrome and never into an application), and
  the console's broker re-authenticates that account, audits the attempt, and
  starts the application as it. The session learns only the pid, or the
  refusal.
- **The launch cannot block the compositor.** A graphical caller cannot use
  the broker's blocking `Run` exchange: its reply arrives only once the
  elevated program has *exited*, so a desktop posting it would stop serving
  windows to the very program it is waiting for — a deadlock. Hence
  `ElevateRequest::Launch`, which is the identical re-authentication with the
  started program's pid as its reply (`plans/CAPABILITY_USE.md` CU5). The
  started program is *login's* child, so login tracks and reaps it; the
  desktop cannot wait on a child that is not its own, and no launch leaves a
  zombie.
- **The prompt fails closed and states its outcome.** An empty field is never
  offered (there is nothing to check, and asking would spend an audited
  attempt against the account). A refusal keeps the prompt up with the reason
  stated and the password cleared — the masked field zeroises what it
  discards — so a retry starts from empty with the account name kept. A
  refused authentication and a program that would not start read
  *differently*, so an accepted account is never blamed on its password. A
  cancellation says on `stderr` that the clock was not changed rather than
  leaving silence.

Tested in the taskbar suite (the reading leads and is non-actionable and
repeats the bar's label; an unset clock states so; the set-time row is denied
with its reason until the session attests, then asks for the typed request;
every row in the table is stated and maps back consistently; a secondary press
on the clock asks for the menu and a primary press asks for nothing) and in the
session suite (the prompt opens
once and refuses a second; Escape cancels and offers nothing; Enter offers
exactly what was typed, once, for the program the prompt named, and reports
the started pid; an incomplete prompt is never offered and moves the keyboard
to the empty field; a refusal keeps the prompt up, states it, clears the
password and keeps the account name, and the retry offers the new password; a
launch failure reads differently from a refused password; abandoning offers
nothing; an idle prompt ignores every event; a primary press on the clock is
claimed by the bar and opens no menu). The wire form and the broker's
decision table are
covered in `lib/abi` and `userland/session/login` respectively
(`plans/CAPABILITY_USE.md` CU5).

Docs: `userland/gui/taskbar/README.md`, `userland/gui/session/README.md`,
`docs/src/desktop/taskbar.md`, `docs/src/desktop/session.md`,
`docs/src/desktop/apps.md`, `docs/src/security/capabilities.md`.

---

## T18 — The pointer's focus: the bar reacts only to input aimed at it — **done**

The bar can see its own geometry and not the window stack, so on its own it
cannot tell a clock the user is looking at from a clock a window is drawn over.
Nothing pins the bar topmost — it is an ordinary compositor window, raised over
by every application window that is opened or clicked — so the two are
routinely different.

What that guarantees now:

- **`SessionInputRouter` is the session's input seat.** It owns the pointer
  position (the desktop's one copy) and resolves, per pointer event, which
  surface holds the pointer: a modal surface of the bar's (an *active grab*),
  else the surface a held button grabbed (the *implicit grab*, ending at the
  last button up), else the window `Compositor::window_at` draws under the
  pointer — the bar's when the `TaskbarPresenter` placed it, the window
  manager's otherwise. It delivers to that one router and to no other.
- **`tairix_input::PointerFocus`** (`Entered { at }` / `Left`) is the derived
  enter/leave pair the seat hands each router. It is not an `InputEvent`: no
  device produces it. A `Left` is the only way a hover can end, because a
  window rising over a hovered control leaves the pointer exactly where it was;
  an `Entered` carries the position because the pointer can arrive without
  moving. An arrival refreshes hover and opens no hover surface — a window
  closing is not a gesture.
- **Both routers implement it**: `TaskbarInput::set_pointer_focus` drops every
  bar hover and starts the window picker's closing grace — an arrival back onto
  the picker's own surfaces cancels it, so a window merely passing over the bar
  does not take the panel down; `InputRouter::set_pointer_focus` puts out the
  title-bar command the pointer was on. `lib/controls` grew the
  position-independent `pointer_left` each needs
  (`WindowControl`, `TitleBar`, `TraySignal`).
- **`DesktopShell::present` re-resolves the focus** before it paints, because
  the answer depends on the stack and every change to the stack ends in a
  present. An in-flight grab pins it, so it cannot interrupt a drag.
- **`desired_cursor(at, …)` and `CursorController::refresh(at, …)`** take the
  seat's pointer position rather than reading a router's cached one: the shape
  must be right over the bar too, which the window manager's router never
  holds.
- **Why it is a security property, not only a correctness one**: the bar is
  trusted chrome whose menus offer to lock the screen, log out, and
  re-authenticate for a privileged application. An unprivileged window that
  could provoke that chrome to open a surface over itself, or have a click it
  received acted on by a control the user could not see, would hold a
  user-interface redressing primitive. One stacking-aware decision point
  denies it. The same rule is why the lock screen is safe: its window is not
  one the presenter placed, so while it is up the pointer cannot reach the bar
  at all.

## Open questions to resolve in review (stop and ask, §15.7)

None outstanding. The settled ones are recorded in their own done-state
sections: the icon-bar menu's transport is inline bounded rows, which is also
`plans/NEW-MENUS.md`'s M0 decision (T7); bar presence is *declared* rather
than derived, so an application keeps its slot for the life of its process
(T6); the information panel is manifest-attested rather than app-supplied, so
no application can spoof an identity in system chrome (T7); the picker opens
only at two windows, and only on a deliberate rest, because a hover-opened
panel that a passing pointer can raise is one nobody asked for (T7);
`CAP_PROC_CONTROL` was minted with its live enforcement point — no earlier
capability fit — and the monitor service is session-spawned, with the capsule
degrading calmly when it is absent (T10).
