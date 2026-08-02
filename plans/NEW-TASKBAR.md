# NEW-TASKBAR.md — the taskbar / icon bar becomes first-class

Binding under `AGENTS.md`. This is the staged build plan that takes the
Stage 7 taskbar (`userland/gui/taskbar`, `tairix-taskbar`) from today's
edge-pinned layout + start-menu-with-session-controls skeleton into the
full **icon bar** the desktop needs:

- a permanent left-most **Program Library** launcher — a first-class,
  folder-organised catalog of installed applications (Accessories,
  Programming, Games, Internet, …), programmatically add/removable exactly
  as an installer adds shortcuts;
- to its right, a permanent **File Manager** icon that opens `files.app`
  on its default view;
- then a user-editable strip of **pinned application shortcuts** (created by
  dragging from the file manager / desktop or by right-click → *Pin to
  taskbar*);
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
(the `files` app + the *Pin to taskbar* context action), `plans/DISPLAY.md`
(the seat/display model), `plans/FIX-DESKTOP.md` (non-blocking async launch),
and `plans/CAPABILITY_USE.md` (CU6 capability sizing). Every rule in all of
them applies here without exception.

**Note:** `abi-v1` is *not* frozen (the standing task direction supersedes
the `AGENTS.md`/`PLAN.md` freeze language until first release). A `lib/abi`
change today is allowed; it requires regenerating the C header
(`cargo xtask c-header --write`), which the drift guard enforces.

## Status

`done` — **T1–T15 complete**, including T15's documentation deliverable and
its QEMU pin + Switchboard vertical.
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
menu is gone. The **pin** layer is landed whole (T6/T7 — see their
done-state sections below): the `lib/taskpins` store, the bar's pin strip +
context menu, the session's pin ownership with the sandboxed per-app icon
pipeline (`lib/image` PNG + `lib/compress` inflate/zlib + the
`lib/sandbox` icon-rasterisation service), and both pin-creation gestures
(right-click *Pin to taskbar* and drag-to-taskbar). The **notification
area** is first-class too (T8 — see its done-state section below): the
versioned, fuzzed `notify_ipc` channel (`lib/abi`) over a seat-scoped
`NOTIFY_ENDPOINT` the kernel binds only for the desktop's live seat lease,
the taskbar's typed status signals + severity-ranked transient-notification
cards (shared `lib/controls`), and the session serving the endpoint —
attesting each producer, relaying raise/clear, presenting the
click-to-dismiss popover, and dropping a dead producer's notifications on
exit. The rest of the starting point is Stage 7 as it stands:
`tairix-taskbar` models the
launchers / popup / pins / task list / notification area / clock and emits
typed `TaskbarResponse`s; `tairix-session` presents the bar, popup, and
menu through the compositor, owns the theme, loads/merges the catalog
stores, and resolves those responses (`plans/FIX-DESKTOP.md` async launch
is done); `lib/controls::switchboard` already renders a `SwitchboardModel`
→ `SwitchboardAction` from the shared Reactive Alloy controls; the `files`
app is a live windowed browser (`plans/APPWIN.md` AW3/AW5). This plan wires
the remaining pieces together and fills the gaps.

## 0. Scope and decisions (binding for this plan)

- **One control implementation, ever (§2.2).** Every visible surface here —
  the library launcher, the file-manager button, pinned-shortcut icons, the
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
  surfaced (§15.7). A foundational primitive (the catalog engine, the pin
  store, the tray-signal feed) is the complete abstraction, not its first
  caller's slice (§27).

## 1. Final bar layout (left → right, horizontal bottom bar)

```
┌──────────────────────────────────────────────────────────────────────────┐
│ [Library] [Files] │ [pin] [pin] … │  running tasks …  │ [tray…] │ [Switch]│
└──────────────────────────────────────────────────────────────────────────┘
  permanent leading    user pins       task list           notif.    always
  (fixed order)        (reorderable)    (existing)          area      right-most
```

- **Leading, fixed order, not reorderable:** `Library` (Program Library
  launcher) then `Files` (file manager). These two are permanent and cannot be
  unpinned or moved.
- **Pinned shortcuts:** a user-ordered, reorderable strip after the permanent
  icons. Empty by default; populated by pin gestures (T7).
- **Running-task list:** the existing `TaskList` region (one `TaskbarItem` per
  top-level window), unchanged in purpose.
- **Notification area:** status icons + transient notifications, left of the
  clock; the clock sits between it and the Switchboard icon (desktop1
  panel 1).
- **Switchboard icon:** always the trailing-most element, reserved, immovable;
  no pin, task, or tray icon may occupy or displace its slot.
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
- `lib/taskpins` **(new, `no_std`)** — the shared **pinned-shortcut store**:
  the per-user ordered pin list, its on-disk grammar, fail-closed parse,
  render, and the add/remove/reorder operations. Kept separate from
  `lib/proglib` because a pin references a library entry / bundle but is
  per-user ordering state, a distinct concern with a distinct store. (T6)
- `lib/abi` — extend `AppInfo` with the optional `library` listing (the
  opt-in folder byte + `library-icon` asset) so the library is *discovered*
  from bundles (T3); add the taskbar↔Switchboard **tray-signal summary**
  record and the **library-edit** / **pin** / **Switchboard-control** IPC
  vocabularies under the usual ABI discipline (versioned, hashed, fuzzed).
- `lib/controls` — add the shared controls the mockups need that do not yet
  exist (resource **Meter**, **PressureRail**, **ActivitySeam**, **SignalBead**
  refinements) so both the taskbar icon and the Switchboard window compose
  them (T9, T11–T14). The existing `switchboard` composition is extended
  (Pressure + Activities sections) in place (§2.13).
- `userland/gui/taskbar` — the leading library+files buttons, the pin strip,
  the reserved Switchboard slot, and the richer notification area (T4, T6, T8,
  T9).
- `userland/gui/session` — the glue: launch library/pin/files bundles, relay
  pin gestures, present the library popup, forward Switchboard open/reveal,
  relay the tray-signal summary to the taskbar (T4–T9).
- `userland/gui/switchboard` **(new)** — the Switchboard component: a
  long-running monitor service that samples the system and publishes the
  tray-signal summary + serves the on-demand overview window built on
  `lib/controls::switchboard` (T10–T13).
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
- Command apps (`/System/Apps`, `plans/APPS.md` §8), background services
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
  rewrite, and the kernel logs the denial. The **per-user** overlay and the
  per-user pin store likewise need no new capability: they are ordinary §5.3
  file-permission writes under the user's own `/Users/<u>/Settings/` identity.
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
  (writable `/System/Settings` subtree, `nosuid,nodev,noexec`, §16.2). One
  line per entry, grammar owned by `lib/proglib`, structurally like
  `lib/sysconfig`: `#` comments, blank lines ignored, each entry a
  fail-closed record of `bundle-path`, `category`, `display-name`, optional
  `icon` asset id, and a stable `id`. Written only by principals the
  `/System/Settings` per-inode policy admits (the system identity).
- **Per-user overlay:** `/Users/<u>/Settings/ProgramLibrary/library.conf` —
  same grammar; lets a user hide, re-file, or rename an entry, and add
  entries for their own `/Users/<u>/Apps` bundles, without touching the
  system store. Merge policy: the user overlay is applied over the machine
  catalog (user hide/rename/re-file wins; user-only entries append). The
  merge is one pure, exhaustively-tested function in `lib/proglib`.
- **Per-user pins:** `/Users/<u>/Settings/Taskbar/pins.conf` — grammar owned
  by `lib/taskpins`: an ordered list of pin records, each referencing a
  library entry `id` (or a bundle path for a pin that is not catalogued),
  plus its display order. Written under the user's own identity (no new cap).
- All three are **untrusted input** to every reader: bounded length, alloc
  discipline per crate policy, fail closed on anything not fully understood
  (unknown key, bad category, duplicate id, oversize), and a reader that
  cannot fully parse runs on an empty store rather than guessing (§2.9, §5.4,
  §24.4 — these are format bounds, not growable capacities).

---

# Staged plan

Each stage is independently reviewable, ends green on the whole-project gate
(§7), and lands its surface fully (§27). Stages are ordered by dependency;
T1–T3 (library data), T4–T5 (library UI), T6–T7 (pins), T8–T9 (tray + icon),
T10–T13 (Switchboard), T14 (fidelity), T15 (docs/gate). T9 needs the T10
tray-signal feed for its live states, so the two land together.

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
- **The path spellings defined once** — `LIBRARY_DIR`, `LIBRARY_FILE`,
  `LIBRARY_SETTINGS_SUBDIR`, `MACHINE_LIBRARY_PATH`, and `user_library_path`
  (over the caller's inherited home, the runtime truth even for a moved
  home). No I/O, no authority.

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
  through `lib/proglib` over the secured VFS under the caller's attested
  identity. The machine store write is gated by its §5.3 per-inode system
  ownership (no new capability — §4 above; the kernel logs the denial);
  `--user` targets the caller's own overlay (`user_library_path` over the
  inherited `HOME`), and `hide`/`show` record the visibility verdict on the
  target store's own entry or as an overlay patch.
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
  the `LibraryCategory` folder) plus an optional `library_icon` asset name
  (legal only on a listed bundle; icon-without-listing, an unknown folder
  byte, or a dirty reserved field refuse the whole manifest). There is no
  `show_in_library` boolean and no app-class heuristic: a bundle asks to be
  listed by declaring its folder, in its own signed manifest. The manifest
  TOML source grows the matching optional `library` / `library-icon` keys
  (composer-validated: unknown folder, case drift, an icon without a
  listing, or a `library` on a `service` fail the build). The C header is
  regenerated (`cargo xtask c-header --write`); `lib/appload` consumers read
  the listing off the verified header's own accessors.
- **The fold (`lib/proglib::Catalog::reconcile`)** + **`applib rescan`** —
  the walk covers `/System/Apps` then `/Apps` (machine) or the caller's
  `<home>/Apps` (`--user`), breadth-first in sorted order (deterministic
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

## T4 — Taskbar permanent leading icons: Library + File Manager — **done**

The bar's leading region is the two permanent, fixed-order launchers.

What now stands:

- `userland/gui/taskbar`: `TaskbarConfig.launcher_extent` sizes the two
  leading slots; `BarLayout.library`/`.files` place them (clipped
  fail-closed to `Rect::EMPTY` on a degenerate screen — never hit);
  `Hit::Library`/`Hit::Files` and the typed
  `TaskbarResponse::OpenLibrary`/`OpenFiles` route a primary press. The
  buttons are `lib/controls` `IconButton`s — Library is the accent-filled
  `Primary` invoker carrying the new `lib/icon` `Library` glyph (a
  three-by-three tile grid), pressed-in while its popup is open; Files is a
  quiet `Neutral` folder glyph — with hover feedback driven through the
  bar's per-surface repaint latch (`Taskbar::take_repaint` →
  `TaskbarRepaint`), so a hover repaints only the surface it changed and
  never a per-frame present. The bar owns a copy of the active `Theme`
  (layout/hit/paint read one definition) and the renderer signature dropped
  its separate theme parameter.
- The generic start menu is **gone** (`StartMenu`/`MenuLayout`/`MenuAction`/
  `SessionControl` deleted, §2.14): the session-control rows were wired to
  nothing in the production session (only the taskbar model and tests
  consumed them), so nothing was lost; their real home arrives with the
  Switchboard's System menu (T13). The appearance toggle left the UI with
  the menu — the decision (owner-confirmed) is **no interim seam**: theme
  switching stays programmatic (`DesktopSession::set_theme`) until T13.
- `userland/gui/session`: `OpenFiles` is resolved **idempotently** — the
  `LaunchTable` records every desktop-launched child's PID + label + spawn
  path (its attested bundle identity; no app-controlled data), so a press
  raises the running file manager's served window (`window_of_pid` via the
  window engine's kernel-attested ownership + `DesktopShell::raise_window`),
  lets an in-flight launch finish undisturbed, and spawns only when no copy
  is alive. `OpenLibrary` presents the T5 popup and re-reads the stores.

Tested in the taskbar suite (layout/hit/scale/edges/degenerate + both
buttons' pixels), the session suite (routing, raise-vs-launch, launch
table), and the AW3/AW4 QEMU vertical, whose pointer script now clicks the
Files button directly and reconstructs the same production layout code.

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
  opens outward on every edge, clamps to the screen, sizes to the rows
  capped by the available space, and **probes** the shared `Panel` chrome
  rather than re-deriving it; the WM rounds the panel with the same radius
  the chrome draws (§2.2). Controls are static renderers, so reduced motion
  holds by construction.
- **Full keyboard model**: `Tab` cycles search↔rows; arrows wrap, Home/End/
  PageUp/PageDown jump with the view following the cursor; Enter/space
  activates (folder toggles, entry launches); Left/Right fold/climb/descend;
  typing anywhere routes into the search (type-to-filter, case-insensitive);
  Enter in the search launches the first match; Escape clears then
  dismisses. While open the popup is modal at both routing layers (the
  taskbar router and the session router): presses, releases, scroll, and
  keys all route in; click-away (any button) dismisses without acting on
  what it hit; WM motion outcomes are discarded so nothing is delivered to
  windows beneath.
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
- **Deliberate deviation, recorded**: the staged text had T5 "offer" the
  right-click *Pin to taskbar* typed action with its session path arriving
  in T7. A typed action emitted before any consumer exists is speculative
  surface (§2.4/§23.3), so the pin affordance — context surface, typed
  action, and session path — lands **whole in T6/T7** with the pin store.
  Right-press inside the panel is claimed (modal) and does nothing today.

Tested in the taskbar suite (rows/sort/hide-empty/placeholders, keyboard
nav, filtering, folds, wheel + scrollbar, dark/light/high-contrast pixel
probes), the session suite (loader fail-closed matrix, modality, launch
flow end-to-end, open-popup refresh), and the AW4 QEMU vertical, which now
opens the popup from the planted machine store and launches the terminal
through its catalog entry (keyed by bundle identity, not display text).

## T6 — Pinned shortcuts: model, store, and taskbar region — **done**

What now stands, and the invariants a future change must keep:

- **`lib/taskpins`** (in §3 + `PLAN.md`; fuzzed by `tests/fuzz_taskpins.rs`)
  is the per-user ordered pin store engine: one line per pin (`entry <id>` /
  `bundle <path>`, `#` comments), stored order = display order, targets
  deduplicated, whole-document fail-closed refusals with the 1-based line
  (`PinsError`), canonical render that round-trips byte-for-byte, fixed
  bounds (`MAX_PINS = 128`, line/document caps derived from the field caps),
  and `PinList` operations `pin`/`pin_at`/`unpin`/`move_pin`/`position`. It
  reuses `tairix-proglib`'s validated `EntryId`/`BundlePath` so a pin's
  reference can never diverge from the catalog's own validation. The store
  is `~/Settings/Taskbar/pins.conf` (`user_pins_path`); there is no
  machine-wide pin store — pins are per-user state only.
- **The bar** (`tairix-taskbar`) has a pin strip between the permanent
  launchers and the task list: `TaskbarConfig::pin_extent`, per-pin slots in
  `BarLayout::pins` (+ `pin_strip`), `Hit::Pin(index)`, and
  `BarLayout::pin_drop_index` (the strip-plus-task-list drop band, indexed
  by slot midpoints — T7's drop target). The session hands it resolved
  `PinView`s (label, class glyph, optional artwork, optional entry id,
  optional matched window); `PinStrip` derives each pin's live
  `TaskVisibility` from the `TaskList` at paint time (Active/Minimized/
  Running; `Closed` for no or stale window — fail closed), so window state
  has exactly one home. Every pin and every task slot paints as the shared
  `lib/controls` `TaskbarItem` — one visual recipe — with two as-built
  control extensions recorded in `plans/GUI-CONTROLS-DESIGN.md` §11.26: an
  icon-only `TaskbarPresentation` for compact slots and the
  `TaskVisibility::Closed` quiet-at-rest state, plus owner-supplied
  pre-rasterised artwork (the control never parses image bytes;
  `TaskbarItem::icon_side` / `Taskbar::pin_icon_side` expose the exact
  drawn geometry). A running task whose window matches a pin borrows the
  pin's icon identity, so one application shows one icon everywhere.
- **Deliberate deviation, recorded**: the staged `PinContext(index)`
  response is not how the context surface landed. The bar owns its one
  right-click menu (`BarMenu`, composed from the shared `Menu` control,
  opened by a secondary press on a pin, modal, outward-opening, presented
  by the session as a third window beside the bar and popup) and emits
  *typed outcomes* instead: *Open* → `TaskActivated` (restore/focus) or
  `ActivatePin { index }` (launch), *Unpin* → `Unpin { index }`. A pin
  press follows the task click rule when its window is live and reports
  `ActivatePin` otherwise. This keeps presentation in the bar and authority
  in the session, exactly like the popup.
- **The session** (`tairix-desktop-session`) owns the store: it loads with
  the library's fail-closed posture (absent → empty; unusable → empty plus
  a loud stderr reason), edits through the one `SessionFileWriter` seam
  (whole-document rewrite; memory adopts an edit only after the write
  succeeded, so memory and disk never diverge), resolves each pin for
  display (an `entry` pin through the merged catalog; a `bundle` pin
  through its own bounded fail-closed `AppInfo` read; an unresolvable pin
  keeps a best-effort identity with no launch path so it can still be
  unpinned), matches running windows through the attested launch table +
  window ownership (never titles), and re-resolves on a dirty latch before
  the next present. `ActivatePin` resolves through the same idempotent
  launch-or-raise rule as the Files button (the shared `activate_bundle`).
- **Per-application icons land here too** (beyond the staged text — task
  direction): a pin's bundle icon (the manifest's `library_icon` asset, SVG
  or PNG) is untrusted third-party input, so the session never decodes it
  in-process. New `lib/image` (complete fail-closed PNG decoder) +
  `lib/compress` `inflate`/`zlib` (RFC 1951/1950 decode) + the
  `lib/sandbox` **icon-rasterisation service** (`iconraster`: SVG via
  `lib/svg`/`lib/icon`, PNG via `lib/image` with alpha-weighted box-filter
  scaling and aspect-fit centring; capped input 256 KiB, side ≤ 512) do the
  decode in a capability-empty worker — the session's own binary re-entered
  in worker mode — and the session verifies, caches (per asset path × side,
  refusals included), and falls back to the shared class glyph on any
  refusal.

Tested in the taskpins suite (grammar round-trip, refusal matrix with
exact lines, operation semantics, fuzz), the taskbar suite (strip layout/
reflow/clipping on all four edges, drop-index mapping, visibility
derivation, pin activation split, menu rows/modality/keyboard/click-away,
artwork and glyph pixel probes, borrowed task artwork), the controls suite
(presentations, Closed state, artwork, icon-side probe), the session suite
(store load matrix, edit persistence + refusing writer, resolution matrix,
service decisions, drag/drop policy, secondary-press menu routing), and
the sandbox suite (the icon service's happy paths, refusals, hostile
replies, and fuzz).

## T7 — Pin gestures: *Pin to taskbar* + drag-to-taskbar — **done**

The two ways a user creates a pin. What now stands:

- **The wire**: the app-window channel gained three ops, evolved in place
  (`abi-v1` unfrozen): `WindowRequest::PinBundle` (6), `DragOffer` (7), and
  `DragWithdraw` (8), each carrying the validated bounded `BundleRef` path
  newtype (`WINDOW_BUNDLE_PATH_MAX = 512`, UTF-8, no control characters,
  zero-tail enforced); `WindowRequest::WIRE_LEN` widened to 530 for every
  op (one fixed frame, the house decode style). All three round-trip and
  fuzz through `lib/abi`'s window-IPC suites; the engine validates window
  ownership before dispatching any of them, and the `WindowHost` bridge
  methods default to **refuse** (`PinDecision::Refused` / `false`), so a
  host that does not serve pinning fails closed.
- **Right-click → Pin to taskbar**: `lib/browse` gained
  `ContextCommand::PinToTaskbar` (ordered after *Open with…*, enabled iff
  the selection is a bundle via the shared `EntryKind` classifier — the
  menu model now carries the selection's kind); the files app dispatches it
  through `WindowClient::pin_bundle` and reports a refusal (already pinned
  / bar full / refused) as one terse stderr line, never fatally. The
  session's `PinBridge::pin_requested` validates fail-closed — store-shaped
  path, decodable manifest — and appends via `lib/taskpins`. The library
  popup's right-click *Pin* rides the same session path as an **entry** pin
  (`PinEntry { entry }` through the bar's context menu, T6), so a
  catalogued app pins by its catalog identity and an uncatalogued bundle by
  its path.
- **Drag-to-taskbar**: the files app's drag source is `lib/browse`'s pure
  `BundleDrag` detector (primary press on a bundle row arms; the first
  motion beyond `DRAG_THRESHOLD_PX = 6` sends exactly one `DragOffer` per
  gesture; `Escape` sends `DragWithdraw`; a release disarms locally; a
  refused offer disarms silently). The session arms at most **one** offer
  (per gesture, keyed to the offering channel window; a new offer replaces,
  a withdraw disarms only its own window). The drop is the shared
  host-tested `resolve_pin_drop` policy: a primary release from the
  offering served window consumes the offer either way; landing on the pin
  band (`BarLayout::pin_drop_index` — the strip plus the task-list region,
  so a first pin lands on an empty strip) re-validates the bundle fully and
  pins at the drop index; anywhere else the gesture simply ends. Dragging
  from the desktop is gated on the desktop-icons work (not yet present) —
  the drop target, payload, and session handling are complete, so the
  desktop is a later *source*, not new taskbar machinery.

Tested in the abi suites (round-trip + refusal matrix + fuzz for the three
ops), the window-engine suite (ownership binding, decision→status mapping,
fail-closed defaults), the browse suites (menu row/order/enable rules; the
drag detector's arm/threshold/one-offer/withdraw semantics), and the
session suite (request validation decisions, drag management, and the
drop-policy matrix: unarmed / unserved window / landing drop persists at
the index / stray release ends the gesture).

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
  so pins, tasks, notifications, and the clock can never displace it (only
  the permanent leading launchers outrank it on a degenerate screen);
  `hit_test` → `Hit::Switchboard`. The mockup microinteractions landed
  (desktop1 panel 6): scroll over the capsule cycles the running tasks
  (wrapping, honest no-op on an empty list), middle-click switches to the
  previous task (an MRU-of-two the task list keeps), hover previews via the
  capsule's instrument readout, and a primary press resolves as a **tap or
  a hold** — a quick release reports
  `TaskbarResponse::OpenSwitchboard { section: Section::Tasks }` (the
  panel's NOW column), a press held past `input::LONG_PRESS_AFTER_NS`
  (500 ms) reports it with `Section::Recovery`, and the readout's one safe
  action, "Open Switchboard", reports the tap's response. One press reports
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
  consecutive-failure tolerance, then a stated exit — never an unbounded
  silent retry (§2.1).

Tested: sampler scope probing and degradation, delta/permille arithmetic,
stopped counting, top-task selection across samples, memory cadence divider;
derive matrix incl. hysteresis boundaries and dominance; publisher
change-only + keepalive + failure exits; schedule deadlines (tickless);
manifest ∩ ceiling behaviour is covered by the grant-intersection kernel
tests it rides on. Docs: `userland/gui/switchboard/README.md`,
`docs/src/desktop/switchboard.md` (+ the session/taskbar pages), `AGENTS.md`
§3, `PLAN.md`.

## T11 — Switchboard window: the Open Panel (desktop1 panel 2, desktop2a §1–2)

**Deliverables**
- `userland/gui/switchboard` hosts the existing `lib/controls::switchboard`
  composition in a real app window (`plans/APPWIN.md` AW2 channel), fed the
  live `SwitchboardModel` from the T10 sampler and emitting `SwitchboardAction`
  the service authorises + applies (pause/resume, switch-to, reveal window,
  quit, force-quit) through capability-checked syscalls; "lower priority"
  stays in T12 with the scheduler surface it needs (§4).
- The T9 gestures now have a target. The **taskbar side stands**: the
  capsule's tap reports
  `TaskbarResponse::OpenSwitchboard { section: Section::Tasks }`, a hold
  past `LONG_PRESS_AFTER_NS` reports `Section::Recovery`, and the readout's
  "Open Switchboard" safe action reports the tap's response. The session
  consumes that response by asking the service to open/raise its window at
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
- The header resource meters (CPU / MEM / DISK / NET) from the mockup are
  drawn with a new shared `lib/controls` **Meter** control (a rounded bar with
  the value trace + **pressure rail** colour by resource), added to
  `lib/controls` (never a Switchboard-private draw, §2.2). The four existing
  sections (Tasks, Jobs, Recovery, Overview) render the NOW / BACKGROUND /
  PRESSURE / RECOVERY / SYSTEM content from live data.
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
bounded rolling CPU sparkline and the pressure the T10 derivation latched.
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

**Status — the shared controls (`lib/controls`): done.** `meter.rs` adds the
`Meter` instrument: one resource reading (label, reading text, rounded track
tinted by the resource's semantic rail through the same `signal_color` lookup
`Card`'s Pressure Rail uses, optional bounded sparkline), read-only with no
input or action. `MeterValue::Unmeasured` makes an unmeasurable resource
unrepresentable as a real zero, so a denied or absent query draws a quiet
groove instead of a fabricated `0%`. The `switchboard` composition tiles one
per resource in an always-visible header band above the Tabs strip (every
region below shifts by its measured height; an empty resource list collapses
it to nothing) and the band routes no pointer or key input. `ResourceSummary`
carries the measured value, pressure and inline bounded history once, feeding
both the band's `Meter` and the Overview `Card`. `select_section` lets a host
open on a chosen section and `set_model` refreshes live data in place —
both through the one internal transition, so the tab strip, content and
per-section scroll can never disagree; a refresh keeps section, offsets,
focus, pointer and any in-flight drag, and deliberately drops row-indexed
selection, hover and any armed press so a press begun on one row can never
complete against its replacement.

**Status — the taskbar side (`userland/gui/taskbar`): done.** The capsule's
primary press resolves as a **tap or a hold** into
`TaskbarResponse::OpenSwitchboard { section }` — tap → `Section::Tasks`, hold
past `LONG_PRESS_AFTER_NS` (500 ms) → `Section::Recovery` — and the readout's
new "Open Switchboard" safe action reports the tap through that same one
route. The hold is resolved from the `now_ns` the embedder passes in (the
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
Recovery, Overview — on the same in-place `SwitchboardModel` (no v2), with
the wire `CommandSection` and the session/service mappings extended in step.

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
capsule** (desktop1 panel 5) and reuses the bar's one modal `BarMenu`
machinery — there is no second popup surface. The taskbar holds no
authority: each row reports a typed outcome and the session resolves it.

**Rows, in order** (`—` marks a group divider):

| Row | Resolved by | Backing |
|---|---|---|
| About This System | session → Switchboard `Overview` | the T11 overview (identity, uptime, load, memory) |
| System Monitor | session → Switchboard `Tasks` | the T11 open panel |
| Task Shell | session → launch `os.tairix.terminal` | the graphical terminal bundle |
| — | | |
| Light / Dark Appearance | session `ThemeRegistry::set_theme` | §10; the active one carries a check bead and is not actionable |
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
  carries `lock_available`, attested by the session from
  `elevate_endpoint(self_origin().console())`. It defaults to refusing, so a
  bar that was never told renders the row non-actionable with the Authority
  Mark and a stated reason rather than stranding the user behind a prompt
  nothing can answer.

**Rows deliberately not shipped, and what each waits on.** A row that cannot
act must not exist, so these are absent rather than present-but-dead:

- **New command** — the session's spawn path passes no argv and the terminal
  bundle accepts no command to run, so there is nothing for the row to
  invoke. It needs an argv-carrying launch path (`plans/APPS.md`,
  `plans/PTY.md`), not a menu entry.
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

**Tests**: the row table renders the expected labels, groups, roles and check
marks for both appearances; a secondary press on the Switchboard capsule opens the menu
and a press elsewhere does not; the menu is modal and a click away dismisses
without acting; keyboard navigation reaches every row; an unpermitted power
row is non-actionable, carries the Authority Mark and states its reason;
power rows are denied when no authority has been published; the lock row is
denied until the session attests it can prompt, and emits nothing while
denied; a launch row with no catalog entry is disabled and emits nothing;
each row maps to exactly the expected typed outcome; the confirmation dialog
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
program library, pins, notification area), `docs/src/desktop/switchboard.md`
(the system-overview surface, feed, sections, actions, capabilities, and the
relayed power transition), `docs/src/desktop/session.md` (every quick-action
outcome and the trusted power-confirmation prompt),
`docs/src/desktop/theming.md` (the live Light/Dark control), and the
`docs/src/lib/` pages for `proglib`/`taskpins`/`controls`/`image` are current
against the built surfaces, as is the crate `README.md` beside the monitor
service. The `README.md` support matrix needs no row: the desktop is
architecture-neutral, and the security matrix's re-authenticated screen-lock
row already covers T13.

**Prerequisite settled while landing this stage.** A pinned shortcut only
survives if the pin store can be written, and `<home>/Settings/Taskbar/` is
two levels below the home while the session's writer creates exactly one
parent. Homes were being created bare — no `Settings/`, no `Desktop/`, none
of the fixed shape — so the very first per-user write of any kind failed
`NotFound` on a real install. The home's shape is now one shared definition
(`tairix_users::{HOME_MODE, HOME_SUBDIRS}`, `plans/USERS.md` U1) that the
`CAP_USER_ADMIN` provisioning path, `tools/mkimage`, and the QEMU users-root
fixture all read, so a pin written in the guest lands on a home shaped like a
real one.

**QEMU vertical: done.** `tests/integration/taskbar_pin_qemu_aarch64` is a
dedicated short vertical rather than a fourth stage on the already long
`autoload_input_qemu_aarch64` choreography, so a gate mis-count in one
cannot wedge the other (D20). It boots the same graphical world (the
`AutoloadRootDisk` image, unlock → login → `desktop`), then opens the
program library, secondary-presses an entry row to raise its context menu,
chooses *Pin to taskbar*, spends one press dismissing the modal popup,
opens the Switchboard from its capsule, and clicks the pin it created.

What holds it together:
- **Every coordinate is the product's own.** The script drives a host
  `DesktopShell` with the very events the guest will receive — through
  `TaskbarInput`, over a `Catalog` rebuilt from the same manifests the image
  plants (`reconstructed_library`) — so the menu it clicks is the menu the
  model opened, its pin row comes from `Menu::row_rect` at the exported
  `MENU_PIN_ROW`, and the pin slot from `Taskbar::layout` with one
  `PinView`. Nothing is a hand-copied pixel.
- **Three PASS witnesses, each attributable to one act**: the audited
  `mkdir` of the pin store's own directory (`PINS_SETTINGS_SUBDIR`, which a
  provisioned home does not carry), the window endpoint serving the panel's
  create *and* first present, and an `APP_LOADED` naming the pinned bundle
  — `widgets`, which no other stage of any vertical launches, and which
  needs no picker, no filesystem, and no ambient authority.
- **The pin click is gated causally, not by a timer.** Serving a
  window-endpoint call is a different wake of the session's serve loop from
  the input wake that pinned, and the session re-resolves its pin strip
  before parking at the end of every wake — so the guest's
  panel-presented marker cannot appear until the pin is live in the bar the
  guest hit-tests, under any host load. (Every earlier step keys on the
  display service's first-present witness alone: the guest applies injected
  events in device order, and the capsule's rect is independent of the
  strip's length.)
- **Two dumps read the bar itself.** The first proves the first pin slot is
  *uniformly* the bar's own plate colour before anything is pinned; the
  second that it now carries the pin's class glyph, that the panel covers
  its cascade slot, and that the column beside the panel is bare desktop.
  The second deliberately does not require the desktop colour to dominate
  the frame — a 760×560 panel covers more than half this output, so that
  would assert something untrue.

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

---

## Open questions to resolve in review (stop and ask, §15.7)

- **Desktop-icon drag source** (T7): depends on the not-yet-present
  desktop-icons work; the taskbar drop-target and payload land now, the
  desktop source is a later, separate source.

(The T10 questions are settled and recorded in their done-state sections:
`CAP_PROC_CONTROL` was minted with its live enforcement point — no earlier
capability fit — and the monitor service is session-spawned, with the
capsule degrading calmly when it is absent.)
