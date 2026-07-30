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

`in progress` — **T1–T3 done**; T4 is next. The library **data** layer is
landed end to end: `lib/proglib` (T1 — taxonomy, entry model, store grammar,
fail-closed parse, canonical render, `merge`, `reconcile`, fuzzed by
`tests/fuzz_proglib.rs`), the `applib` admin command (T2 —
`userland/apps/applib`), the manifest `library` listing + `applib rescan`
discovery, and the image-build catalog seeding (T3 — `tools/mkimage`).
Everything else below (T4 onward — the UI) is unstarted. The rest of the
starting point is Stage 7 as it stands:
`tairix-taskbar` models the start button / task list / notification area /
clock and emits typed `TaskbarResponse`s; `tairix-session` presents the bar
through the compositor, owns the theme, and resolves those responses
(`plans/FIX-DESKTOP.md` async launch is done); `lib/controls::switchboard`
already renders a `SwitchboardModel` → `SwitchboardAction` from the shared
Reactive Alloy controls; the `files` app is a live windowed browser
(`plans/APPWIN.md` AW3/AW5). This plan wires those together and fills the
gaps.

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
- **Notification area:** status icons + transient notifications, immediately
  left of the Switchboard icon.
- **Switchboard icon:** always the trailing-most element, reserved, immovable;
  no pin, task, or tray icon may occupy or displace its slot.
- Vertical / top / right edges reflow along the cross axis by the existing
  `Edge`/`Orientation` model; "left/right" above is main-axis leading/trailing.

The session controls that live on the start menu today (Log Out, Lock, Shut
Down, Restart) **move into the Switchboard's system quick-actions menu**
(desktop1 panel 5, T13). The leading icon is repurposed from a generic "start
menu" into the **Program Library launcher**; the appearance (light/dark)
toggle likewise moves under Switchboard → System.

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
  reprioritise, quit/force-quit). This plan **checks first** for an existing
  signal/priority/kill capability (the process-control syscalls `plans/SPAWN.md`
  / the signal work): if one exists at an appropriate granularity it is used;
  a new `CAP_PROC_CONTROL` is minted **only** if none does, and only in T10
  alongside the service that enforces it, subject to §5.2 review. Every
  control action is a capability-checked syscall/IPC under the Switchboard
  component's own identity — never ambient (§4) — and every allow/deny is
  audit-logged (§19.4).

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
T10–T13 (Switchboard), T14 (fidelity), T15 (docs/gate) — but T8 (notification
area) and the file-manager button in T4 have no dependency on the library
data and may proceed in parallel.

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

## T4 — Taskbar permanent leading icons: Library + File Manager

Rework the taskbar's leading region from "one start-menu button" into the two
permanent, fixed-order icons.

**Deliverables**
- `userland/gui/taskbar`: extend `TaskbarConfig`/`BarLayout` with the two
  leading slots (`Library`, `Files`), each an `IconButton` (`lib/controls`)
  drawn with a `lib/icon` glyph. `hit_test` returns a `Hit::Library` /
  `Hit::Files`; `TaskbarInput` turns a primary press into a typed
  `TaskbarResponse::OpenLibrary` / `TaskbarResponse::OpenFiles`. Neither is
  reorderable or removable; both survive a degenerate size fail-closed
  (`Rect::EMPTY`, never hit).
- The former generic "start menu" is retired: session controls and the
  appearance toggle move to Switchboard → System (T13). The library popup
  (T5) replaces the start-menu popup; the `StartMenu`/`MenuLayout` code is
  either repurposed into the library popup or deleted (§2.14 — no dead code).
- `userland/gui/session`: resolve `OpenFiles` by async-launching the `files`
  bundle on its **default view** (`plans/APPWIN.md`/`plans/NEW-FILEMANAGER.md`)
  — if a files window is already open, raise it instead of spawning a second
  (idempotent open). Resolve `OpenLibrary` by presenting the library popup
  (T5).

**Tests**: layout places Library then Files at the leading end at every scale
/ edge / orientation; hit-test + input routing produce the right typed
responses; degenerate size fails closed; session launches/raises files
idempotently (host test over the async-launch seam).

**Done when**: the two permanent icons render, route, and open their targets;
gate green.

## T5 — Program Library popup (folder-organised launcher)

The clickable, folder-organised launcher the Library icon opens.

**Deliverables**
- A `lib/controls`-composed popup surface (a `Panel` of `MenuItem`/`ListRow`
  folders, each expanding to its sorted entries; a `ScrollBar` when it
  overflows; optional `SearchField` to filter by name). It is presented as a
  compositor popup window by the session (like today's start-menu popup), NOT
  a taskbar-private widget. Full keyboard navigation, focus fields, and
  reduced-motion per `plans/GUI-CONTROLS-DESIGN.md`.
- The popup is built from a merged `Catalog` (`lib/proglib::merge` of machine
  + user stores) the **session** reads and hands to the popup as typed view
  models (the taskbar/popup never touches the VFS). Selecting an entry emits
  a typed `LibraryLaunch { entry_id }`; the session resolves the bundle path
  and async-launches it (`appmgr`, `plans/FIX-DESKTOP.md`), reporting a
  refusal loudly (`userland/gui/session`'s launch-failure path).
- Right-click on a library entry offers *Pin to taskbar* (T7) as a typed
  action; no launch happens on that path.
- Empty folders are hidden; an empty library shows a calm "No applications"
  state, never an error.

**Tests**: folder/entry rendering from a fixture catalog; sort + hide-empty;
keyboard nav; launch emits the right entry id; denied/failed launch is loud;
search filter; both themes + high-contrast.

**Done when**: the library opens, browses by folder, launches apps, and
offers pin — all from disk data; gate green.

## T6 — Pinned shortcuts: model, store, and taskbar region

**Deliverables**
- `lib/taskpins` (new `no_std` crate; README stability, added to §3 +
  `PLAN.md`): the ordered pin store grammar (parse/render/fail-closed), and
  `PinList` operations `pin`/`unpin`/`move` (reorder), each referencing a
  library entry id or a bundle path. Host-tested + fuzzed parser.
- `userland/gui/taskbar`: a **pin strip** region between the permanent icons
  and the running-task list. Each pin is a `TaskbarItem` (`lib/controls`) with
  its icon+label identity and, when a matching window is open, the
  Running/Active/Minimized `TaskVisibility` state (so a pinned app that is
  also running shows its live state, Windows-11-style). `hit_test` →
  `Hit::Pin(index)`; primary press → `TaskbarResponse::ActivatePin(index)`
  (launch if not running, else activate/minimise per the existing rule);
  right-press → `TaskbarResponse::PinContext(index)` (unpin / open).
- `userland/gui/session`: owns the `PinList` (read/write the per-user store
  under the user's identity), builds the taskbar's pin view models, resolves
  `ActivatePin`/`PinContext` (launch/raise/unpin), and re-presents on change.

**Tests**: pin/unpin/reorder round-trip through the store; a running pinned
app shows live `TaskVisibility`; activate launches-or-raises; context unpins;
layout reflows as pins are added/removed; degenerate size fails closed.

**Done when**: pins persist per-user, render with live state, launch/raise,
and can be unpinned; gate green.

## T7 — Pin gestures: *Pin to taskbar* + drag-to-taskbar

The two ways a user creates a pin (issue requirement).

**Deliverables**
- **Right-click → Pin to taskbar** from the file manager and from the library
  popup: `userland/apps/files` (`plans/NEW-FILEMANAGER.md`) adds a context
  action on a `.app` bundle that sends a typed *pin request* (bundle path +
  suggested name/icon) to the session over the app-window channel
  (`plans/APPWIN.md` AW2 — a new versioned, fuzzed request kind); the session
  validates it (the bundle exists and is launchable) and adds the pin via
  `lib/taskpins`, failing closed on a bad path. The library popup's
  right-click *Pin* (T5) uses the same session path.
- **Drag-to-taskbar**: define the WM drag payload for an app reference (a
  typed, versioned `lib/abi` drag record carrying a bundle path). The taskbar
  advertises the pin strip as a **drop target**; a drop over it (routed by the
  session, which owns both the WM and taskbar routers) creates a pin at the
  drop index. Dragging from the desktop is gated on the desktop-icons work
  (not yet present) — the drop-target + payload + session handling land now so
  the file-manager drag source works immediately and the desktop source is a
  later source, not new taskbar machinery (§2.19 — the mechanism is complete,
  only one *source* is pending and is noted here).

**Tests**: files context action produces a valid pin; a bad/absent bundle
path is refused fail-closed; a drop on the pin strip pins at the right index;
a drop elsewhere is ignored; the drag payload round-trips + fuzzes.

**Done when**: a user can pin an app by right-click and by dragging from the
file manager; gate green. *(Desktop-icon drag source: pending desktop-icons
work; tracked here.)*

## T8 — Notification area upgrade

Bring the right-side notification area to first-class Reactive Alloy, left of
the reserved Switchboard slot.

**Deliverables**
- `userland/gui/taskbar`: render the notification area's status icons through
  `lib/controls` (the existing `NotificationArea` model feeding `IconButton`/
  `TraySignal` presentation) — network, volume, battery, and the clock — plus
  transient notifications drawn as the `lib/controls::shell` `Notification`
  card when raised. Icons resolve artwork from `/System/Graphics` (the
  session loads assets; the taskbar draws them).
- A versioned, fuzzed notification IPC (`lib/abi`) a producer service uses to
  raise/clear a notification; the session relays to the taskbar model.
- Positioned immediately left of the Switchboard slot; reflows fail-closed.

**Tests**: icon layout/hit-test; a raised notification renders as a card and
clears; severity ordering; both themes + high-contrast + reduced-motion.

**Done when**: the notification area shows live status + notifications through
shared controls; gate green.

## T9 — The Switchboard taskbar icon (always right-most, immovable)

**Deliverables**
- `userland/gui/taskbar`: reserve the trailing-most slot for the Switchboard
  icon. It is laid out **after** everything else and can never be displaced;
  `hit_test` → `Hit::Switchboard`, primary press →
  `TaskbarResponse::OpenSwitchboard`, and the mockup microinteractions
  (desktop1 panel 6): scroll to cycle tasks, middle-click to switch to
  previous, hover preview, long-press to open recovery.
- The icon is a `lib/controls::shell` `TraySignal` driven by a compact
  **tray-signal summary** (the T10 feed): its state renders Normal / Job
  Active (badge count) / Resource Pressure / Hung App (danger) / Recovery
  Available exactly as the mockup's icon-states row, using **signal beads**,
  the **pressure rail**, the **heat seam**, **danger state**, and **edge
  wake** from the Reactive Alloy vocabulary. Any new visual (the badge count,
  the pressure rail on the icon) is a shared `lib/controls` addition, not a
  taskbar-private draw.
- `userland/gui/session`: subscribe to the Switchboard service's tray-signal
  summary and feed it to the taskbar; resolve `OpenSwitchboard` by asking the
  Switchboard service to open/raise its window (T10/T11). If the service is
  absent, the icon shows the calm Normal state and the click is a no-op with
  a logged reason (fail closed, never a crash).

**Tests**: the reserved slot is always trailing-most and never displaced by
pins/tasks/notifications; each tray-signal state renders its beads/rails;
microinteractions route correctly; absent-service degrades calmly.

**Done when**: the immovable Switchboard icon renders every live state and
opens the overview; gate green.

## T10 — The Switchboard component: monitor service + tray-signal feed

The dedicated, capability-sized process behind the Switchboard (§0).

**Deliverables**
- New `userland/gui/switchboard` bundle (§16.5) with its own `AppInfo`
  requesting exactly `CAP_SYSINFO_GLOBAL`/`CAP_SYSINFO_KERNEL`/`CAP_SYSINFO_HW`
  (read) + the process-control authority (§4 capabilities) + the D7 window
  class (`CAP_DISPLAY`-adjacent app-window channel). It is a long-running
  component started with the desktop session; its manifest declares no
  `library` folder, so the program library never lists it.
- A **sampler** that reads the live system through `lib/procinfo` / `sysinfo`
  on a **tickless one-shot timer** and on demand (never a busy-poll, §2.23):
  process/task list, CPU/mem/disk/net stats, per-CPU times, pressure signals,
  hung-task detection, recovery candidates.
- The **tray-signal summary**: a compact, versioned, fuzzed `lib/abi` record
  (overall state Normal/JobActive/Pressure/Hung/Recovery + badge counts)
  published to the session for the taskbar icon (T9). Published on change
  (event-driven), not polled.
- Defines/uses the process-control capability (§4 above) and audit-logs every
  control decision (§19.4).

**Tests**: sampler builds a `SwitchboardModel` from a fake `sysinfo` source;
tray-signal summary derives the right state/counts; tickless (no busy-loop);
control action is capability-checked + audited + fails closed.

**Done when**: the service samples the system tickless, publishes the tray
signal, and holds only its sized capabilities; gate green.

## T11 — Switchboard window: the Open Panel (desktop1 panel 2, desktop2a §1–2)

**Deliverables**
- `userland/gui/switchboard` hosts the existing `lib/controls::switchboard`
  composition in a real app window (`plans/APPWIN.md` AW2 channel), fed the
  live `SwitchboardModel` from the T10 sampler and emitting `SwitchboardAction`
  the service authorises + applies (pause/resume, switch-to, reveal window,
  lower priority, quit, force-quit) through capability-checked syscalls.
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

## T12 — Pressure view + Activities grouping (desktop1 panels 3–4, desktop2a §2–3)

**Deliverables**
- Extend the `lib/controls::switchboard` `SwitchboardModel` **in place**
  (§2.13 — no v2) with a **Pressure** section ("why is my machine slow": a
  per-resource `Card` with a plain-language cause, a **pressure rail**, a
  **heat seam** for live rate, and recommended actions — Sleep app / Show
  memory / Throttle / Lower priority) and an **Activities** section (group
  related tasks into an activity to focus/pause/snapshot/hibernate/close as a
  set). Both are new sections + view models on the existing typed model; the
  Tabs strip gains the entries.
- The pressure causes and recovery recommendations come from the T10 sampler
  (measured, not guessed, §2.16); actions are `SwitchboardAction`s authorised
  server-side.

**Tests**: pressure cards render per resource with the right rail/seam;
recommended actions emit + authorise; activity group focus/pause/close emits
the right set actions; danger posture on a destructive action.

**Done when**: pressure and activities panels are live and actionable; gate
green.

## T13 — System menu / quick actions + System Settings access (desktop1 panel 5)

Where System Settings lives (issue requirement: **not** in the library).

**Deliverables**
- The Switchboard's SYSTEM section / quick-actions menu (desktop2a §4 +
  desktop1 panel 5): About, System monitor, Task shell, New command;
  **Services, Permissions, Configure** (System Settings surfaces); and the
  session/power controls **Lock, Log Out, Restart, Shut Down** — the controls
  that used to live on the start menu (T4). Rendered with `lib/controls`
  `Menu`/`MenuItem`/`Button`; destructive/power actions carry the danger +
  confirmation posture (`Dialog`).
- System Settings itself is reached here (Configure → the settings surfaces),
  invoking the existing `configure` path (`lib/sysconfig`) and the
  permissions/services tools under their capabilities. Settings is a Switchboard
  responsibility, never a program-library folder.
- The appearance (light/dark) toggle (retired from the start menu in T4) lands
  here too, resolved through the session's `ThemeRegistry` (§10).

**Tests**: menu renders + routes each action; power actions confirm and
fail closed on denial; Configure opens the settings surface; appearance toggle
re-themes the desktop.

**Done when**: the system menu exposes tools, settings, and session/power
controls; System Settings is reachable only here; gate green.

## T14 — Reactive Alloy fidelity pass

Verify the full design vocabulary is present and correct across taskbar +
Switchboard, adding any missing shared control to `lib/controls`.

**Vocabulary → where it appears** (all from `lib/controls`, §2.2):

| Reactive Alloy element | Surfaces |
|---|---|
| **Pressure rail** | Switchboard resource meters + pressure cards; the Switchboard tray icon under pressure |
| **Live seam** | task rows with live CPU/IO activity |
| **Signal badge / bead** | tray icon job/alert/recovery counts; notification & recovery items |
| **Edge wake** | scrolled task lists wake the action column on the edge |
| **Danger state** | hung-app icon, force-quit/power actions, destructive dialogs |
| **Heat seam** | background-job progress; pressure live-rate |
| **Focus field** | the current section/row soft focus glow |
| **Action beads** | summary counts on action groups |
| **Magnetic motion** | controls stay aligned/grounded as the system moves (respecting reduced-motion) |

**Deliverables**: audit each surface against `plans/GUI-CONTROLS-DESIGN.md`
§5/§9/§11/§14/§15; ensure dark/light, high-contrast shape fallbacks, and
reduced-motion for every state; every new visual is a shared control with its
own tests, never a per-surface draw.

**Done when**: every listed element is present, themed, high-contrast, and
reduced-motion correct, with tests; gate green.

## T15 — Documentation, integration tests, and the validation gate

**Deliverables**
- `docs/src/desktop/`: update/author `taskbar.md` (the icon-bar layout,
  program library, pins, notification area) and add `switchboard.md`
  (the system-overview surface, data feed, actions, capabilities); update
  `docs/src/lib/` pages for `proglib`/`taskpins`; rustdoc on every new public
  item (§2.8, §13). Update the `README.md` support matrix if a per-arch state
  changes (§13).
- A QEMU integration vertical: boot the graphical session, open the library,
  launch an app, pin it, open the Switchboard, and screendump-verify the bar +
  Switchboard render (the D7-style host-side proof).
- Run the whole-project gate (§7): `cargo fmt --all`, `cargo xtask ci` (once),
  `cargo xtask fuzz --secs 5`, and `tools/ci/soak.sh both --secs 20`; fix any
  failure in the same change.

**Done when**: docs current, integration vertical green, whole-project gate
green.

---

## Open questions to resolve in review (stop and ask, §15.7)

- **Process-control capability**: does an adequate signal/priority/kill
  capability already exist (`plans/SPAWN.md` / the signal work)? If yes, reuse
  it; only mint `CAP_PROC_CONTROL` in T10 if none fits (§5.2).
- **Switchboard lifecycle**: started by the session at desktop bring-up vs. a
  `/System/Services` autostart — decide in T10 against `plans/DISPLAY.md` and
  the CU6 sizing rule; the taskbar icon must degrade calmly when it is absent.
- **Desktop-icon drag source** (T7): depends on the not-yet-present
  desktop-icons work; the taskbar drop-target and payload land now, the
  desktop source is a later, separate source.
