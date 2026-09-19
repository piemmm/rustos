# NEW-DESKTOP-SETTINGS.md — the desktop Settings application

Binding under `AGENTS.md`. This is the staged build plan for **Settings**
(`os.tairix.settings`), the windowed application that configures the desktop
and the machine, opened from the *Settings…* row of the Switchboard capsule's
system quick-actions menu.

Read first, in order: `AGENTS.md` (all of it, §2, §4, §5, §10, §16.5, §17.4),
`plans/GUI-CONTROLS-DESIGN.md` (the Reactive Alloy vocabulary every surface
here composes — no second control implementation), `plans/NEW-SWITCHBOARD.md`
(the sibling system surface, and the boundary in §0 below),
`plans/NEW-TASKBAR.md` T13 (the system quick-actions menu this is launched
from), `plans/APPWIN.md` (the window channel, the app-owned popup surfaces),
`plans/APPS.md` (§2 bundle layout, §2.1 the `Help/` locale tree, §14 the
mandatory app icon), `plans/APPDATA.md` (the per-app store the session's
settings live in), `plans/PINBOARD.md` §6 (the apply-rendezvous pattern every
user-scope write here reuses), `plans/ICONS.md` (§0, the mandatory built-in
glyph tier), and `plans/CAPABILITY_USE.md` (capability sizing). Every rule in
all of them applies here without exception.

**Note:** `abi-v1` is not frozen. A `lib/abi` change is allowed and requires
regenerating the C header (`cargo xtask c-header --write`), which the drift
guard enforces.

## Ledger

Every stage this plan calls for, what it waits on, and where it is specified.
A stage is `done` only when its host tests, its rustdoc and `docs/` page, and a
green whole-workspace gate landed with it. Nothing here is optional: a stage
dropped is a category the surface then has to lie about.

| # | Stage | Depends on | Spec | Status |
|---|---|---|---|---|
| **DS1** | `lib/controls::form` — `FieldRow`/`FieldGroup`/`FieldControl`/`FieldLayout` over the row chrome hoisted into the shared `paint` core, the measured-width accessors the slot model needs, the caption-line badge, and a `widgets.app` gallery tab | — | DS1, §4 | done |
| **DS2** | The `userland/apps/settings` crate and its shell: the closed `Category`/`Pane` registry, the vertical `Tabs` sidebar, the search index, the breadcrumb band, frame shedding, the absence-pane renderer, and the taskbar's *Settings…* row | DS1 | DS2 | done |
| **DS3** | Appearance and Accessibility over the session's user-scope appearance registry, and the apply rendezvous every other user-scope write reuses | DS2 | DS3 | done |
| **DS3b** | The cursor pair: a cursor-set store under `/System/Graphics/Cursors/<set>/` so `cursor.set` has a choice space at all, and a `cursor.size` factor in the session's cursor controller | DS3 | DS3b | done |
| **DS4** | Wallpaper — the gallery absorbed into the pane over two served requests, `wallpaper.app` deleted, and *Change Background…* opening Settings at that pane | DS3 | DS4 | done |
| **DS5a** | The shared volume view model: the mount record → capacity/health derivation in `lib/procinfo`, the `VolumeHealth` banding in `lib/abi`, the one band→role binding in `lib/theme`, and every private copy collapsed onto them | — | DS5a | done |
| **DS5** | Storage — one card per mount with its capacity track and health pill, over the DS5a model | DS2, DS5a | DS5 | done |
| **DS6** | The elevated-apply seam: `ElevateRequest::Run` gains a bounded argv, and General (About, Login & startup, Caching, Date & Time) is its first consumer | DS2 | DS6 | done |
| **DS7** | Networking read — per-interface facts, link state, addresses and rates through the Switchboard's own client, plus the stack-wide `net.*` options | DS2, DS6 | DS7 | planned |
| **DS8** | Networking write — `configure` grows the `lib/netconfig` registry, Ethernet and DNS stage and apply through it, and the stack adopts the change without a reboot | DS6, DS7 | DS8 | planned |
| **DS9** | Users & Groups — the ungated `GROUP_DIRECTORY` sibling, the caller's own record, the admin-authenticated read of every other account, and the user-admin operations the syscall carries but no tool spells | DS6 | DS9 | planned |
| **DS10** | Notifications — a per-source allow/deny and minimum severity enforced at the session's one `NotifyRequest` intake | DS3 | DS10 | planned |
| **DS11** | Keyboard and Mouse — the session's pointer and key-repeat policy, and the one double-click interval it publishes for every app | DS3 | DS11 | planned |
| **DS12** | Lock Screen and Screensaver — the session's single idle deadline and the one timer armed only while a policy has one pending | DS3 | DS12 | planned |
| **DS13** | The `settings_qemu_aarch64` vertical and the docs pages the surface owes | DS2–DS12 | DS13 | planned |
| **DS14** | Retire the second form idiom — `datetime.app`'s six-field row and `lib/browse`'s `PermGrid`, with the private layout arithmetic each carries deleted | DS1 | DS14, §6 | in progress — `datetime.app` landed with DS1 (its grid deleted, its extent now measured through `Dialog::height_for_content`); `PermGrid` remains |

**The honest shape of the deliverable.** Seven of the categories the desktop
should offer have no subsystem beneath them today: there is no audio stack, no
Bluetooth stack, no print/scan stack, no touchpad or touch input driver, no
802.11 driver, and no file/screen sharing server anywhere in the tree. Settings
cannot invent them, and it must not draw a volume slider that changes nothing —
that is the fabricated-reading defect the whole desktop is built to avoid. So
those categories are **present, reachable, and honest**: each states what is
missing and what would have to land, exactly as the Switchboard's Services and
Power pages already do. §3 is the table of them; each row names the plan that
would fill it. Every other category is backed by a real reading and a real
write path on landing.

---

## 0. Scope and decisions (binding for this plan)

- **Switchboard observes; Settings changes.** The two surfaces are not
  siblings with overlapping content and must never become one. The
  Switchboard reports what the machine *is doing* — tasks, pressure, faults,
  live per-core load, mounted volumes' health — and its commands act on
  running things (raise, pause, force, restart a task). Settings changes what
  the machine and the desktop *are configured to be*, and holds no command
  that acts on a running task. Where both want the same fact, the fact has
  one reader: the shared System Information API query. Where both would want
  the same *command*, only one has it — the machine's power transitions stay
  in the system quick-actions menu, behind its existing confirmation, and
  Settings' Power pane does not offer a second route to them
  (`AGENTS.md` §2.2, and macOS parity: shutting down is not a settings pane
  there either).

  Concretely, the one reader is `lib/procinfo`: it already owns the paged
  walks over `MOUNT_LIST`, `USER_DIRECTORY`, the process list, the CPU-time
  stats and the pressure fetch, and both the Switchboard and the CLI tools
  read through it. Settings adds no sampler of its own, and no derivation
  either: a mount record becomes a capacity reading in `lib/procinfo`'s
  `volume` module (DS5a), which every surface reads, rather than each
  keeping a private copy that will drift.

- **Settings holds no domain authority. It never holds any.** Its manifest is
  `CAP_CONSOLE_WRITE` + `CAP_SHM`, and nothing else — the same class as the
  widget gallery. It does **not** hold `CAP_TIME_SET`, `CAP_NET_ADMIN`,
  `CAP_USER_ADMIN`, `CAP_SYSTEM_POWER`, `CAP_STORAGE_ADMIN`, `CAP_DISPLAY`, or
  `CAP_FS_MOUNT`. It does not hold `CAP_USERS_READ` either, which is the
  sizing decision worth naming: that capability gates `users_db_read`, whose
  answer is the whole `users-v1` credential database **including every
  password record**. A settings browser that wanted to print a user's full
  name would be holding every hash on the machine. So the account roster comes
  from the ungated, credential-free `USER_DIRECTORY` query instead (DS9).
  This survives absorbing the wallpaper gallery (DS4), which is the one place
  it came under real pressure: browsing pictures needs the store listed and
  each one decoded, and the chooser held `CAP_FS_ACCESS`, `CAP_PROC_SPAWN` and
  `CAP_LOG_EMIT` to do it. Settings does not inherit them. The session already
  owns a sandboxed image renderer, so it serves the catalog and the previews
  and Settings asks — which keeps the manifest at two capabilities *and*
  leaves one sandboxed decode path on the desktop instead of two.
  An application that could change everything is precisely the ambient-
  authority god-app `AGENTS.md` §4 and §5.2 forbid, and a settings *browser*
  does not need to be one: every change is either a request to the process that
  already owns that domain, or a re-authenticated run of the tool that already
  writes that store. §2 is the authority map, pane by pane. A pane whose write
  path is refused states the refusal and changes nothing (`AGENTS.md` §2.24) —
  it never reports a success it did not get.

- **Three write paths, and no fourth.** Every settable in this plan reaches
  one of exactly three owners:

  1. **User scope → the desktop session.** Appearance, contrast, density,
     reduced motion, UI scale, cursor set, wallpaper and pinboard keys,
     notification policy, pointer and idle policy. Settings renders the
     document and posts it to the session, which validates it, applies it, and
     persists it to its own published app-data scope. This is the
     `plans/PINBOARD.md` §6 rendezvous exactly as the backdrop menu already
     uses it: the session is the only writer, an application publishes
     only its own scope, and the desktop adopts a change **only after the
     write succeeded**, so memory and disk cannot diverge. That write happens
     on the session's settings worker, never on its serve loop, and the same
     rule binds this application's own panes: a control's value is never wired
     to a write, a continuous control acts durably only where its interaction
     settles, and no pane ever blocks its window on a store (`AGENTS.md` §28).
     A slider that posted its document per pointer sample would freeze both
     this window and the desktop's.
  2. **Machine scope → the tool that already writes that store, run as an
     authenticated account.** `system.conf` and `network.conf` have exactly
     one writer engine each (`lib/sysconfig`, `lib/netconfig`) and one command
     app over them (`configure`). Settings does not grow a second writer: it
     asks the console's elevation broker to re-authenticate an account that
     may and run that same program (DS6). The CLI and the GUI are then
     literally the same writer and cannot diverge.
  3. **Kernel scope → the syscall's own tool, run as an authenticated
     account.** Users, groups, grants and passwords go through the
     `users_admin` syscall, whose gate is `CAP_USER_ADMIN`; the clock goes
     through `CAP_TIME_SET`. Both are reached by elevating the tool that owns
     them — the user-admin command family and `datetime.app` — never by
     Settings acquiring the capability.

- **A pane is a form, and the form family is shared.** A settings pane is a
  scrollable column of captioned groups of label/description/control rows.
  That shape is not this app's to invent privately: the file manager
  hand-rolled a permissions grid, the wallpaper surface a column of four
  drop-downs, and `datetime.app` a row of six fields. `lib/controls::form` is
  the one family (DS1). `datetime.app` is converted (DS14), the wallpaper
  copy went with the application that carried it (DS4), and `PermGrid` is
  what remains — because two form idioms in one desktop is the duplication
  `AGENTS.md` §2.2 forbids. The family composes the existing row chrome and control families; it
  re-implements no plate, press, focus, disabled or Authority-Mark rendering.

- **The pane registry is one closed table, and it is data.** `Category` and
  `Pane` are closed enums; one ordered `CATEGORIES` table is the single
  definition of the sidebar, the search index, the location trail, the
  keyboard cursor, and the pane dispatch. A pane cannot exist without a row,
  or a row without a pane. Adding a category is adding a row and a renderer,
  never touching the shell.

- **Absence is stated, never mimed.** Three distinct statements, and the
  surface never blurs them, because they are different facts to a reader:
  *no interface exists in TAIRiX yet* (the pane says so and names what would
  have to land); *the interface exists but this machine has no such hardware*
  (an empty list with the absence named); *the interface and the hardware
  exist but this caller may not change it* (the control keeps its value and
  wears the Authority Mark, with the reason stated). A control that would
  change nothing is never drawn as though it would.

- **Every reading is a measurement.** Settings reads the live machine through
  the System Information API and the config-store engines, never through a
  pseudo-file and never through a remembered value it hopes is still true. A
  reading it could not take renders unmeasured, never a fabricated zero or a
  default presented as the truth. It re-reads on window focus and after every
  applied change, so what it shows is what is.

- **One window, one instance, and no icon-bar slot.** Settings is part of the
  desktop rather than an application the user manages: its signed manifest
  presents no icon-bar slot, so closing the window ends the program — there is
  no slot left holding a handle on a windowless process. It is a singleton,
  which is the manifest's own default and is what makes *Change Background…*
  able to reach the Settings a user already has open (DS4) rather than
  starting a second view of one machine's configuration, each able to
  overwrite the other's applies.

- **One window, resizable, server-decorated.** The compositor draws the title
  bar, frame, and window commands; Settings' content is the whole client
  (`plans/COMPOSITOR-WORK.md`). It re-maps its zero-copy frame region on
  `WindowEvent::Resized` and lays the shell out to the new viewport. No modal
  maze: a pane's every control is on the pane, and the only overlays are the
  shared `Menu`/`ComboBox` popups and one `Dialog` for a destructive
  confirmation.

- **Fail closed, park never poll.** The event loop parks on the wait set; a
  pane that is not on screen samples nothing; a refused read leaves the pane
  exactly as it was and states why; a malformed or refused apply changes
  nothing anywhere.

- **Not in this plan:** the audio, Bluetooth, print, touch, wireless, and
  sharing subsystems themselves (§3 names each one's prerequisite); the
  compositor's window furniture; display-mode setting (there is no mode-set
  request in `display_ipc`, §3); civil time zones (`plans/TIMEZONES.md`);
  and the service manager (`plans/NEW-SERVICEMANAGER.md`). This plan consumes
  those surfaces where they exist and states their absence where they do not.

---

## 1. The surface

### 1.1 Chrome and navigation

```
 ┌──────────────────────────────────────────────────────────────────┐
 │ [search field]      │  Settings › Networking › Ethernet          │  band
 ├─────────────────────┼────────────────────────────────────────────┤
 │ ⚙ General           │  ┌──────────────────────────────────────┐  │
 │ ◑ Appearance        │  │ CONNECTION                           │  │
 │ ▤ Wallpaper         │  │  Status              Connected       │  │
 │ ▭ Displays          │  │  Configure IPv4      [DHCP      ▾]   │  │
 │ ⚿ Lock Screen       │  │  IP address          10.0.2.15      │  │
 │ ◔ Screensaver       │  └──────────────────────────────────────┘  │
 │ ⏻ Power             │  ┌──────────────────────────────────────┐  │
 │ ⇅ Networking      ▸ │  │ DNS                                  │  │
 │ ᛒ Bluetooth         │  │  Servers             10.0.2.3    [+] │  │
 │ ♪ Sound             │  └──────────────────────────────────────┘  │
 │ ⌨ Keyboard          │                                            │
 │ …                   │                              [ Apply ]     │
 └─────────────────────┴────────────────────────────────────────────┘
```

- **The sidebar** is `tabs::Tabs` in `TabsOrientation::Vertical` — the control
  the Switchboard's System section already uses for exactly this job, turned
  on its side, not a second selection model. Each row carries its category's
  `IconKind` glyph and its label. A category holding more than one pane shows
  a trailing chevron and expands in place; the expanded pane rows are rows of
  the same strip, so one cursor walks the whole column.
- **The search field** sits above the sidebar (`text::SearchField`) and
  filters the strip to the categories and panes whose label, pane title, or
  *setting* label matches — the index is derived from the one `CATEGORIES`
  table plus each pane's declared setting labels, so a searchable setting
  cannot exist without a row that shows it. Matching a setting selects its
  pane and scrolls that row into view, which is cheap because the registry
  already knows every label.
- **The location band** carries a `nav::Breadcrumb` reading
  `Settings › <category> › <pane>`. Its trailing crumb is the current
  location and is inert; the leading crumbs are the route back, which is what
  makes a narrow window navigable with the sidebar shed. The band is a
  Tab-cycle focus region.
- **Region shedding.** One resolver (`shell::resolve_frame`) resolves the
  band, sidebar, and content once per layout; the paint and the hit test both
  read it, so a press can never land on a control drawn elsewhere. A window
  too narrow to seat the sidebar sheds it — the breadcrumb's leading crumb
  then opens the category list as a `Menu`, exactly the Switchboard's
  section-list idiom — and the content column always survives.
- **The content column** is a vertical stack of `FieldGroup`s under the one
  shared `ScrollBar`. A pane taller than the viewport scrolls; the band and
  sidebar do not.
- **The cursor.** Tab cycles band → sidebar → content → footer. Within the
  sidebar, Up/Down walk categories and panes; within the content, Up/Down walk
  rows and Enter/Space commits the focused row's control. Every control is
  reachable without a pointer, and a refused control refuses the keyboard
  exactly as it refuses the pointer.

### 1.2 Applying a change

Two postures, declared per setting in the registry, never improvised:

- **Immediate** — the change is cheap, reversible, and its effect is the
  feedback: appearance, contrast, density, reduced motion, UI scale, cursor
  set, wallpaper fit, icon flow, notification policy. The row commits on
  interaction; the pane re-reads and shows what took effect. There is no Apply
  button, because there is nothing to batch and a stale Apply is a trap.
- **Staged** — the change is a document a service must validate, or it needs
  re-authentication: the `net.*` and `cache.*` registries, an interface's
  addressing, an account's fields. The pane edits a working copy, shows which
  rows differ from what is in effect, and offers **Apply** and **Revert** in
  the pane footer. Apply posts the whole document (or asks for the one
  elevated run) and reports the outcome in the footer; a refusal leaves the
  working copy intact so the user can correct it rather than retype it.

A pane never mixes the two: a setting is immediate or it is staged, and the
registry says which, so a reader learns the rule once.

---

## 2. The authority map

One row per pane. `read` is what backs the pane's readings; `write` is the
owner the change goes to; the last column is what a refusal looks like.

| Category → pane | Read | Write goes to | On refusal |
|---|---|---|---|
| General → About | `SYSTEM_IDENTITY`, `UPTIME`, `CPU_INFO`, `MEMORY_TOTAL` | — (read-only) | reading renders unmeasured |
| General → Login & startup | ungated `SYSTEM_CONFIG`, parsed by `lib/sysconfig` | elevated `configure` | working copy stands, refusal stated |
| General → Caching | ungated `SYSTEM_CONFIG`, parsed by `lib/sysconfig` | elevated `configure` | working copy stands, refusal stated |
| General → Date & Time | `WallClockReading` | elevated `datetime.app` (launched) | refusal stated, clock untouched |
| Appearance | the session's published settings document | session apply (merged over what it holds) | apply refused, stated on `stderr`, row reverts |
| Wallpaper | session's published settings document; the store catalog and each preview served by the session | session apply (merged) | apply refused, stated; a preview that did not arrive draws its placeholder |
| Displays | `SEAT_LIST`, `DesktopInfo`, `Compositor::window_scale` | session apply (scale only) | mode change: no interface (§3) |
| Lock Screen | session's lock policy document | session apply | apply refused, stated |
| Screensaver | session's idle policy document | session apply | apply refused, stated |
| Power | — | — (no policy interface, §3) | pane states absence |
| Networking → Ethernet | `NET_INTERFACE_FACTS`/`_STATE`/`_RATES` | elevated `configure` (DS8) + netstack reload | Authority Mark, config unchanged |
| Networking → Wi-Fi | — | — | pane states absence (§3) |
| Networking → DNS | ungated `NET_RESOLVER_SERVERS` (the live aggregated set) | elevated `configure` (DS8) | Authority Mark |
| Networking → TCP/IP | `lib/sysconfig` `net.*` | elevated `configure` | Authority Mark |
| Bluetooth | — | — | pane states absence (§3) |
| Sound | — | — | pane states absence (§3) |
| Notifications | session's notification policy document | session apply | apply refused, stated |
| Keyboard | one built-in US layout (`lib/hid`) | session apply (repeat/double-click) | layout: no registry (§3) |
| Mouse | `lib/cursor` registry, session pointer policy | session apply | apply refused, stated |
| Trackpad | — | — | pane states absence (§3) |
| Touchscreen | — | — | pane states absence (§3) |
| Printers & Scanners | — | — | pane states absence (§3) |
| Accessibility | the session's published settings document (contrast, density, motion, scale) | session apply (merged) | apply refused, stated on `stderr`, row reverts; cursor size: no interface (§3) |
| Language & Region | the bundle `Help/` locale set, `lib/sysconfig` | elevated `configure`; zones → `plans/TIMEZONES.md` | Authority Mark |
| Sharing | — | — | pane states absence (§3) |
| Users & Groups | ungated `USER_DIRECTORY` / `GROUP_DIRECTORY` roster + own record; other accounts' fields, lock state and grants only after admin authentication (DS9) | elevated user-admin tool (DS9) | Authority Mark, account unchanged |
| Storage | ungated `MOUNT_LIST` alone — its `VolumeStats` and its availability overlay; `VOLUME_IO_HEALTH` needs `CAP_SYSINFO_KERNEL` and stays the Switchboard's | — (read-only; mounting is the file manager's) | reading renders unmeasured |

**The one rule behind the table.** Settings never performs a privileged
operation. It renders state, and it hands a typed intent to the process that
holds the authority — the session for the user's own desktop, the console's
broker for anything the machine owns. Nothing in this app can be tricked into
an escalation, because there is no capability in it to escalate with.

---

## 3. What has no interface yet

Each row renders a pane that states the absence in words and names the
prerequisite. None of these is stubbed, faked, or drawn as a control that
would change nothing.

| Pane | What is missing | Prerequisite |
|---|---|---|
| Sound | the `audio-v1` stream ABI and the `audiochan-v1` device channel exist; nothing serves them — no driver, no mixer, no device to enumerate | `plans/SOUND.md` SND3–SND4: the `lib/audio` engine and the `audiod` mixer/router with its first driver |
| Bluetooth | no HCI transport, no host stack, no pairing store | a new `plans/BLUETOOTH.md` |
| Printers & Scanners | no print spooler, no scan API, no driver class | a new `plans/PRINTING.md` |
| Trackpad | no touchpad driver; `lib/hid` carries boot-mouse only | a multitouch HID driver under `plans/USB.md` |
| Touchscreen | no touch input path from device to seat | the same, plus a touch event kind in `lib/abi::input` |
| Sharing | no SMB, VNC/RDP, or HTTP server in the tree (`userland/net/` is `netstack` alone) | a new `plans/SHARING.md` |
| Networking → Wi-Fi | no 802.11 driver, no supplicant, no scan/associate vocabulary | a new `plans/WIRELESS.md` |
| Displays → resolution, rotation, arrangement | `display_ipc` has `Query`/`Configure`/`Present` only: no mode *list* and no mode *set* | a mode-enumeration and mode-set request in `display_ipc`, plus driver support |
| Power → sleep, battery, thermal | no power-supply, battery, or sensor interface, and no driver to serve one | `plans/DEVICES.md` sensor work + an ACPI/PSCI sleep path |
| Keyboard → layout, modifier remap | exactly one hard-coded US ANSI table (`lib/hid::console`) | a layout registry (`lib/keymap` grows the data; the seat selects) |
| Keyboard → shortcuts | no shortcut registry anywhere; each surface owns its own keys | a desktop-wide binding registry |
| Language & Region → time zone | no zone data, no local rendering | `plans/TIMEZONES.md` |
| General → Software Update | no updater, no package store | out of scope for this plan |
| Accessibility → screen reader, zoom, sticky keys | no assistive-technology surface | out of scope for this plan |

Two of these are cheap enough to build *here* rather than defer, and this plan
builds them because the categories are useless without them: the **idle
interface** the Lock Screen and Screensaver panes need (DS12 — one timer the
session arms only while idle, never a poll), and the **pointer/keyboard policy**
the Mouse and Keyboard panes need for double-click interval, primary-button
swap, and key repeat (DS11 — the session already routes every event, so it is
already the owner). Everything else in the table stays absent and honest.

---

## 4. Controls this surface adds to `lib/controls`

Generic, reusable, and complete on landing — every state, both appearances,
the heavier-contrast and monochrome paths, pointer and keyboard, damage
reporting, and a `widgets.app` gallery tab, exactly as every other family.

- **`form::FieldRow`** — one setting: a leading label, an optional secondary
  description line, and a trailing slot holding one control (a `Toggle`,
  `ComboBox`, `Slider`, `TextField`, `Button`, or a plain read-only value).
  It composes `collection::ListRow`'s row chrome for hover, selection, focus
  ring, and the leading rails rather than restating any of it, and it renders
  the three absences of §0 distinctly: plainly disabled, Authority Mark, or a
  stated unmeasured value. Under a narrow width the description truncates
  first, then the label; the control keeps its room, because the control is
  what the reader came for.
- **`form::FieldGroup`** — a captioned plate holding rows, with an optional
  footnote beneath (where a setting needs a sentence of consequence, not a
  tooltip). Rows share one column model so every control in a group lines up,
  and a group draws one plate rather than nesting a plate per row.

DS2 added to that: the one `ComboBox::popup_rect` drop-down placement rule
over the shared `plate_rect` (retiring the three private copies in the widget
gallery, the wallpaper surface and the Switchboard), `FieldGroup::layout` as
its ready-made application for a form owner, and the vertical `Tabs`
sidebar-list anatomy above — a leading glyph, a disclosure chevron, one level
of nesting, and the entry-wise scroll (`set_first`, `seated`) a long list
needs.

Everything else a pane needs already exists:
`Toggle`, `Checkbox`, `Radio`, `ComboBox`, `Slider`, `TextField`, `Button`,
`Tabs` (the sidebar), `SearchField`, `Breadcrumb`, `ScrollBar`, `Menu`,
`Dialog`, `FactList` (read-only panes), `MetricTile` (Storage's capacity
tracks), `StatusPill` (a volume's health band, a link state), and
`ActionRail` where a pane commands a selected subject. **No new control is added for a job an existing one does.**

New `IconKind` glyphs, one per category, each with the mandatory first-party
built-in vector glyph so the sidebar can never blank: `Settings`, `Appearance`,
`Wallpaper`, `Display`, `LockScreen`, `Screensaver`, `Power`, `Bluetooth`,
`Sound`, `Notifications`, `Keyboard`, `Mouse`, `Trackpad`, `Touchscreen`,
`Printer`, `Accessibility`, `Language`, `Sharing`, `Users`, `Storage`.
`Network` is reused as-is for Networking. Two of the new kinds share the
artwork of the reading they stand beside rather than drawing a second copy of
it — `Sound` draws `Volume`'s speaker and `Notifications` draws `Bell`'s
bell — keeping an asset slot of their own so a theme may distinguish the
settings category from the tray reading. `Users` and `Storage` draw marks of
their own, because a group of accounts is not one account and a machine's
storage is not one drive.

---

## 5. Stages

Each stage is one fully-gated increment: it lands with its host tests, its
rustdoc and `docs/` page, and a green whole-workspace validation gate (the
whole `AGENTS.md` §7 sequence, not `cargo xtask ci` alone), and — where the
behaviour is only observable end-to-end — extends the QEMU vertical rather
than a faked run. A stage that turns out larger than one clean increment is
split and staged here, never shipped half-done.

### DS1 — `lib/controls::form`: the form-field family

`FieldRow` and `FieldGroup` per §4, over the existing row chrome, plate,
metrics, and state vocabulary, with a `widgets.app` gallery tab and the
family's specification in `plans/GUI-CONTROLS-DESIGN.md` beside the other
control families. Two pieces of shared machinery land with it, because the
family composes rather than restates: the row chrome `ListRow` and `TableRow`
already share moves out of `lib/controls`' `collection` module into its
`paint` core so all three rows draw one recipe, and `Button`, `ComboBox` and
`Toggle` each gain the measured-width accessor the slot model asks them for —
none carried one, so the slot column had nowhere to come from but a second
copy of each control's own layout arithmetic. A combo's figure is the width of
its *widest* choice, so choosing a different value never moves the column.

The contract it delivers, which no later stage re-derives:

- **Room is given out control, label, description.** The slot is reserved
  first, the label elides into what remains, and the description draws only
  while the label fits whole. A slot never exceeds half the row's content
  (`form::slot_ceiling`), so a label always has room to be read.
- **A row's disposition is the setting's.** `FieldRow::set_state` shares
  enablement, authority and validation — exactly what decides actionability —
  with the control in the slot, so a denied or pending row cannot hold an
  actionable control. A pane therefore states a refusal by setting the
  *row*, never by remembering to set two states in step.
- **The owner places the choice popup.** `FieldGroup::popup_anchor` names the
  row and slot to anchor an expanded `ComboBox` list to; the owner places it,
  hands it back through `FieldLayout::with_popup`, and paints it with
  `render_popup` after every group. The pane (DS2) owns the placement rule,
  because only it knows the viewport the list has to fit in — and DS2 is where
  the one `ComboBox` placement rule is hoisted, retiring the three private
  copies (the widget gallery, the wallpaper surface, the Switchboard's task
  grouping) rather than adding a fourth.

### DS2 — the Settings shell, every category reachable, nothing faked

`userland/apps/settings` (`tairix-settings`) is the bundle: a signed
`AppInfo.toml` declaring `os.tairix.settings`, `kind = "application"`,
`library = "SystemTools"` and `capabilities = ["CAP_CONSOLE_WRITE",
"CAP_SHM"]` with its own SVG master in `Resources/`, a `Help/` tree in every
required locale, a `README.md`, and a `build.rs` mirroring the sibling apps'
`freestanding` cfg so the shell is host-testable and `Run` is a freestanding
program. Its two hand-maintained pins — the harness's discovered-bundle list
and the kernel's capability-request registry — carry it as a
`WINDOWED_APP_REQUEST` application.

What the shell guarantees, which no later stage re-derives:

- **The registry is the surface.** `Category` (21) and `Pane` (27) are closed
  sets and `registry::CATEGORIES` is the single definition of the sidebar
  strip, the search index, the location trail, the keyboard cursor and the
  pane dispatch. Its tests hold totality in both directions, so a category
  cannot exist without a row or a row without a pane, and adding a category is
  adding a row and a renderer.
- **Every pane declares what backs it**, and the two answers are the two
  different facts a reader needs: `PaneBacking::None` names what this system
  does not have and what would have to exist, `PaneBacking::Elsewhere` names
  what the pane will show and where the setting is read or set today. Both
  draw through the one `statement` renderer, quiet and with no plate — the
  shape every other stated absence in the desktop takes. A pane that composes
  no controls declares no setting labels, so a searchable setting cannot exist
  without a row that shows it.
- **`frame::resolve_frame` is the one division of the client.** The paint and
  the hit test both read it. A client narrower than the sidebar plus
  `CONTENT_FLOOR` sheds the sidebar *and* the search field — there being no
  strip left to filter — and the leading crumb then lists the categories as a
  shared `Menu`, placed by the one plate rule. The content column always
  survives.
- **The strip is the vertical `Tabs` sidebar list**, which gained the anatomy
  this needs and `lib/controls` lacked: a leading `IconKind` glyph resolved
  through the owner's artwork lookup, a disclosure chevron stating a
  category's own posture, one level of nesting for a disclosed pane, and the
  entry-wise scroll the strip's docs already promised an owner but gave it no
  way to perform. Twenty-one categories want some 700 physical pixels at the
  reference density, so a short window cannot seat them all: the strip gets a
  gutter of its own — carved out of the strip's column, never the pane's — and
  the cursor, a selection and a search result each scroll themselves into
  view. A category the reader cannot reach is a category they cannot open, so
  this is a correctness property rather than a convenience.
- **The search index is derived, not held.** `strip_rows(open, query)` filters
  the table by a category's label, a pane's title, or a setting label a pane
  declares, folding ASCII case; a category reached by its own label offers
  every pane, one reached through its panes offers exactly those, and a query
  that reaches nothing lists nothing.
- **The launch row** is `SystemAction::Settings` in the taskbar's one
  `system::ROWS` table, at the head of the appearance group, mapped onto the
  bar's existing `TaskbarResponse::LibraryLaunch` and resolved against the
  catalog through the same `installed` predicate *Task Shell* uses — so the
  session gained no new launch path, and an absent bundle renders
  non-actionable with `REASON_NOT_INSTALLED`.

### DS3 — Appearance and Accessibility: the session's user-scope document

The first *writing* panes, and the template for every other user-scope write.
What it guarantees, which no later stage re-derives:

- **One document, two groups.** `lib/wallpaper`'s settings registry — now the
  desktop's, not the pinboard's, and its type renamed `DesktopSettings` to say
  so — grew `appearance`, `contrast`, `density`, `motion` and `scale` beside
  the five backdrop keys, on the same closed-key, tolerant-read,
  canonical-render discipline and the same one format engine.
  `SettingsKey::PINBOARD` and `SettingsKey::APPEARANCE` partition it, and a
  test holds the partition. One document because one owner and one published
  scope: the session writes both in one round trip.
- **An apply merges; it does not replace.** Two surfaces edit the desktop and
  neither shows every setting, so a surface renders only the keys it edits
  (`document_of`) and the session lays them over what it holds (`merge`). The
  old whole-document post would have made every wallpaper change reimpose the
  appearance the chooser happened to open on. A refused document is refused
  whole, on a copy, so nothing half-applies.
- **The axes reach the pixels, and every application.** `Contrast`, `Density`
  and `Motion` moved into `lib/abi::desktop` beside `Appearance` (the ABI owns
  a vocabulary that crosses the window channel; `lib/theme` re-exports rather
  than restating), `DesktopInfo` grew all three, and `adopt_desktop` applies
  them in the one call every app already makes. `ThemeRegistry` gained the
  `Accessibility` overlay — `active()` is the theme *as drawn*, `selected()`
  the theme as registered — and `Theme::with_axes` is the single place an axis
  reaches a pixel. **Density was doing nothing at all** before this stage:
  `Metrics::at_density` now derives the three spacing metrics that decide how
  much room a control is given, so the row is a real setting rather than a
  control that would change nothing.
- **One adopt path, bring-up included.** `PinboardChange` split into
  `BackdropWork` and `AppearanceWork`, and `adopt_appearance` is the one place
  the appearance half is put into effect — re-theme, rescale, republish. The
  session's boot-time settings load drives the very same function, so a stored
  `appearance = light` is in force before the first frame rather than ignored
  until the user changed something.
- **The panes are one row definition seen twice.** `appearance::Setting` holds
  each settable's label, sentence, choices, read and write; Appearance adds
  light/dark, Accessibility groups the rest the way a reader looking for them
  would, and the registry's `settings` labels are asserted equal to what the
  composition actually draws. `PaneBacking::Composed` is the third answer a
  pane can give, and the statement renderer draws nothing for it.
- **Asked for, never written, and never on the loop.** The apply client
  (`ApplyOutcome`, and `apply` behind `lib/wallpaper`'s `rt` feature) is one
  definition shared with the chooser, and Settings drives it from a worker:
  the session answers only once its store has been written, so an inline call
  would freeze the window for a disk commit. The rows show the choice at once
  and adopt the durable value when the answer lands, so a refusal reverts.

**The cursor pair is DS3b's, and it landed there.** DS3 left Accessibility
stating that the desktop kept no pointer size; DS3b replaced that statement
with the POINTER group's two real rows.

### DS3b — the cursor pair

`cursor.set` and `cursor.size` joined `SettingsKey::APPEARANCE` and the
Accessibility pane's POINTER group. What it guarantees, which no later stage
re-derives:

- **The pointer is rasterised to a pixel *side*, not to a factor of its own
  design grid.** `VectorCursor::rasterise(side)` replaced
  `rasterise(scale_percent)`/`footprint`, because the grid is an authoring
  detail — 32 units for a built-in cursor, `tairix_svg::DESIGN_GRID` for a
  decoded one — so a factor gave a different pointer size per set and
  swapping sets would have resized the pointer. **That was a live defect, not
  a refactor:** the first shipped SVG set would have drawn a 2048-pixel
  arrow. `CURSOR_BASE_SIDE_PX` is the logical reference side, and
  `Scale::scale_length` is still the one logical-to-physical conversion.
- **The cache epoch is the pixel side and the set, not the scale, the size
  and the set.** An image depends on how many pixels across it is and which
  set it came from and nothing else, so two (scale, size) pairs resolving to
  one side correctly share one cached image. The controller owns the pointer's
  *logical* side (the user's own choice) and never the scale (the output's).
- **`CursorSetId` is an owned bounded name**, `tairix_theme`'s — held inline
  (`lib/inline`'s `ArrayString`) so the epoch stays `Copy` and no allocation
  reaches the compositing path, and validated as a plain leaf name within
  `tairix_abi::desktop::CURSOR_SET_NAME_MAX` because it is spliced into a
  store path. The name *is* the chooser's label, verbatim, as a wallpaper
  category's is. It lives in `lib/theme` — the cursor *vocabulary* crate,
  whose only dependencies are themselves dependency-free — rather than in
  `lib/cursor`, so the settings registry can hold one without linking a
  rasteriser.
- **The store is `<set>/<asset-id>.svg`, a categorised graphics family.**
  `GraphicsFamilyKind::Cursor` is the third family, discovered from
  `lib/cursor/assets/` by the same `GRAPHICS_FAMILIES` walk, and validated as
  it is discovered: an asset name no kind asks for, a set directory no chooser
  could offer, an over-large file, or two files claiming one kind fails the
  *build*. `tools/xtask` additionally decodes and rasterises every shipped
  asset, so artwork that draws nothing or puts its hotspot outside itself is a
  build error rather than a broken pointer.
- **One set ships: `High Visibility`** — a dark pointer under a wide white
  halo, its own bolder geometry rather than a recolour of the built-in
  tables. With the always-present built-in `Standard` that is a choice of
  two, which is what makes the row a control rather than a list of one.
- **Every set is loaded at bring-up; activating one reads nothing.** The
  session walks the store once (`/System` is read-only, so the choice space
  is fixed for the boot) and registers all of them. A settings apply arrives
  on the loop that owes the user a frame, so activation had to be pure memory
  — the alternative was a directory read on that loop, or a whole second
  worker desk for nine small files.
- **A stale set is a legal value.** `cursor.set` names a set; it does not
  assert one exists. A stored choice outlives the image that shipped it, so a
  set an update removed still parses (the rest of the document survives),
  is still *offered* under its own name (opening the pane changes nothing),
  and falls back to the built-in artwork at activation.
- **A third served request, of `QueryDesktop`'s posture.** `QueryCursorSets`
  is seat-scoped, capability-free, and a read: Settings holds no
  `CAP_FS_ACCESS`, so the session lists and Settings asks, exactly as for the
  wallpaper catalog. Unlike `QueryWallpapers` it carries **no operand and no
  paging**: at most `CURSOR_SETS_MAX` short names, which one reply frame
  holds outright, so there is no paging to get wrong.
- **`AppearanceWork` gained a `cursor` flag.** The pair moves the pointer and
  nothing else: a re-theme would repaint every surface and a rescale would
  move every length, for a change only the pointer can see.

### DS4 — Wallpaper: the gallery absorbed, and `wallpaper.app` deleted

Wallpaper is a *section of Settings*, not an application beside it. The
picture gallery is in the Wallpaper pane, `userland/apps/wallpaper` is gone,
and the backdrop menu's *Change Background…* row opens Settings at that
pane.

**Settings still holds no domain authority, and the gallery did not change
that.** Listing the shipped store needs `CAP_FS_ACCESS` and decoding an
untrusted picture needs the sandbox worker a `CAP_PROC_SPAWN` holder hosts —
which is exactly why the old chooser held them. Granting all three to the
application that will later carry Networking, Users and Storage is the
ambient-authority god-app §0 exists to prevent, so the gallery is served
rather than hosted: the desktop session already owns a sandboxed image
renderer, and Settings asks it.

- **Two descriptive window-channel requests**, of the same posture as
  `QueryDesktop`: seat-scoped, capability-free, describing the caller's own
  desktop and granting nothing. Both are *reads*, so §0's "three write paths
  and no fourth" still holds — the only write is the existing session apply.
  - `QueryWallpapers { from }` answers a **page** of the flat catalog, with
    the catalog's total so a caller knows whether to ask again. The session
    walks the store **once, at its own bring-up** — `/System` is read-only,
    so the catalog is fixed for the life of the boot — through
    `catalog_categories`, `catalog_entries` and `lib/wallpaper`'s
    `desktop_catalog`, and answers from memory. That is what keeps a
    directory walk off the compositing loop while leaving the query as
    ordinary and synchronous as `QueryDesktop`. Paging exists because the
    channel's reply frame is bounded; the bound on the catalog itself is
    `MAX_WALLPAPER_CATALOG_ENTRIES`, applied to the whole flat list because
    that bound is a gallery's rather than one directory's.
  - `RenderWallpaper { window_id, shm_handle, index, side }` renders one
    candidate square into a region **Settings** created and granted, which is
    the one thing its `CAP_SHM` already lets it do. It names a **catalog
    position**, never a path, so it cannot make the session read a file the
    caller chose. Its reply is only the acceptance — the read and the
    sandboxed decode happen on the session's existing wallpaper worker, off
    its loop — and it concludes with a `WindowEvent::WallpaperRendered`
    echoing the index and the side, so an answer cannot be adopted for the
    wrong tile. There is one sandboxed decode path on the desktop instead of
    two, and no picture is decoded in the address space of the application
    that browses them.
- **One preview in flight, and the backdrop first.** The session's
  `WallpaperDesk` takes its own backdrop before any preview, so the picture
  the user is looking at never waits behind a thumbnail, and refuses a second
  preview while one is rendering — a bound on how much decoding any set of
  clients can set going. **Nothing is recalled:** a render already taken
  cannot be, and every accepted one answers exactly once, so a window that
  closes mid-render costs one wasted decode into a region only the desktop
  still maps and the slot frees itself. Recalling would mean a second record
  of which preview is in flight, and two records of one fact are a fact that
  can disagree with itself; the loop therefore holds the *mapping* alone and
  the desk holds the request.
- **The pane.** The four pinboard settings (fit, backdrop, icon flow, sort)
  are one `FieldGroup` from the same `Setting` registry as Appearance's,
  fixed at the top of the column and posting `SettingsKey::PINBOARD` through
  DS3's merge, so they cannot disturb the appearance keys. A `Composition`
  now names the key group it renders, which is what makes that true of every
  pane rather than of this one. The gallery is an `IconTile` collection over
  `lib/browse`'s shared wrapping grid, filling the rest of the column and
  scrolling in tile lines; each picture is requested, never awaited, and a
  paint draws what has come back and a built-in glyph for what has not. A
  refusal is remembered, so a picture the desktop will not render is never
  asked for again — including across a scale change, which re-asks only for
  the pictures it has.
- **A picture the catalog does not hold** — one in effect before it was
  removed from the store — is still offered and still selectable. It has no
  catalog position, so it cannot be rendered and its tile draws its glyph and
  its name. That is the one capability lost with the chooser's
  `CAP_FS_ACCESS`, and it costs a thumbnail rather than a choice.
- **`userland/apps/wallpaper` is deleted**, with its bundle, manifest,
  resources, `Help/` tree in every locale, and README. The candidate model,
  the "no picture" entry and the backdrop palette are re-expressed in the
  pane; the gallery's wrapping and hit-test geometry was already
  `lib/browse`'s and is simply composed. Every reference went in the same
  change: the harness's discovered-bundle list, the kernel's
  capability-request registry, the QEMU fixtures, `SETTINGS_RUN_PATH` /
  `SETTINGS_LABEL` replacing `WALLPAPER_*` in the session, `PLAN.md`,
  `plans/PINBOARD.md`, `docs/src/desktop/pinboard.md`,
  `docs/src/lib/wallpaper.md`, and the §15.18 jump-sheet. The `Help/` trees
  and the bundle set are build-discovered, so both followed the directory.
- **A launch may name a place inside an application.** *Change Background…*
  goes through the desktop's one launch funnel with `LaunchTarget::Pane`. It
  is a **third** target form rather than a `LaunchTarget::Path`, because the
  two existing forms are both authority over a *file*: a path is resolved
  under the application's own authority, which Settings has none of, and a
  document is a one-shot delegation. A pane is neither — a name resolved
  against the closed pane registry, conferring nothing, and an unknown one
  leaves the window on the pane it already showed. Singleton is the signed
  manifest's default, so a running Settings is handed the pane and navigates;
  a fresh one is given the same pane as its one argument and opens on it.
  Each pane row carries a stable `name` for this, separate from its `title`
  because a title is what a reader reads and may be reworded.
- **The second form idiom DS14 tracks lost one of its three instances here**,
  by the surface carrying it ceasing to exist.

Host tests: the gallery's candidate model, one picture asked for at a time, a
refusal never re-asked, a malformed answer refused rather than drawn, a
pending tile drawing its placeholder, a press released away from its tile
choosing nothing, a choice leaving every other pinboard value alone, and an
adopt putting a refused choice back; the pane names being unique and
resolvable and an unknown one resolving to nothing; the session desk's
backdrop-first, one-at-a-time and self-freeing rules; and the window
channel's catalog page and render accept/refuse paths.

### DS5a — the shared volume view model

The mount record → capacity/health derivation lives in one home every
surface reads, rather than in the Switchboard's private `resource_report`.

- **`lib/procinfo::volume`** owns the reading→facts conversions: `VolumeBytes`
  (`total`/`free`/`available`, with `used`, `usable`, `used_permille`, and a
  saturating `plus` fold), the two availability spellings
  (`availability_marker` for a `mount(8)` listing, `availability_name` for a
  fact list), `medium_name`, and `volume_health_name`.
- **`lib/abi`** carries the banding beside the state it bands:
  `MountAvailability::health` → `VolumeHealth` (`Healthy`/`Degraded`/
  `Failing`). Derived, never transmitted, and monotone in the existing
  `severity` ranking, so folding a stack with `worse_of` and banding agrees
  with banding each layer and taking the worst.
- **`lib/theme`** carries the one band→`SignalRole` binding
  (`SignalRole::for_volume_health`), because it owns the role vocabulary and
  a CLI tool must not link a theme to print a mount table.

**Two shares, both named, neither the other's default.** `used_permille` is
of the whole medium — what a capacity bar means — while `df`'s GNU `Use%` is
of `usable()`, what a caller may actually allocate. A withheld reserve is
unallocated to both numerators but part of the medium to only the first, so
`df` reads higher on a reserved format; it keeps the GNU definition it is
bound to and reads it off the same model.

Its callers: the Switchboard's storage pane (which bands health through
the shared `VolumeHealth` alone, and whose capacity block names its rows
`Capacity` and `Available` because the two figures differ), `sysmon`'s mount
panel, `fstree`'s two volume walks, `stress`'s scratch-space probe, and
`df`'s `byte_figures`. None keeps a copy, and `fstree`'s own `VolumeSpace`
is gone: it held the same two facts.

### DS5 — Storage

The per-medium used-space overview, in `userland/apps/settings/src/volumes.rs`:
one **card** per mount walked from `MOUNT_LIST` through
`lib/procinfo::for_each_mount`. A card is a `FieldGroup` captioned with the
volume's name (`mount_name_bytes` — its source, else its mount point), with
the banded `VolumeHealth` as a `StatusPill` toned through
`SignalRole::for_volume_health` as the group's **badge**
(`FieldGroup::with_badge`, added with this stage — the group places it,
because it is the only thing that can also take the room out of the caption
and out of the band's height, so a long volume name is cut rather than drawn
under the capsule), rows for mount point / filesystem / device /
medium / availability, and beneath it a `MetricTile` whose
`MetricInstrument::Track` is `VolumeBytes::used_permille`. Both plates are one
scrolled unit, so a capacity can never be on screen without its volume.
Read-only: mounting and unmounting are the file manager's and `mount`'s, and a
second route to them is duplication.

**A volume that reports no capacity has no tile at all** — the card carries a
`Capacity` row of `FieldControl::Unmeasured` instead, so the in-RAM layout
mounts never draw a full bar or an invented percentage. A field the table left
empty is likewise `Unmeasured`, never a blank a reader would take for a
reading. The row labels are one list (`VOLUME_FACTS`), read by the registry's
search index and drawn by the card, so a term that reaches the pane reaches a
row it shows.

**The health pill is the mount table's own availability, not the gated
counters.** Settings holds neither `CAP_SYSINFO_KERNEL` nor
`CAP_SYSINFO_GLOBAL`, so `VOLUME_IO_HEALTH`'s bucketed completions are the
Switchboard's alone. It does not need them: `MOUNT_LIST` is ungated and its
record already carries the live availability overlay a failing or recovering
device sets. The manifest stays `CAP_CONSOLE_WRITE` + `CAP_SHM`. The band is a
summary, so the *exact* state stays on its own row — "recovering" and
"degraded" must not collapse into one word.

**The mount walk is an IPC round trip, so it never runs on the loop that owes
a frame.** It is read at bring-up beside the picture catalog and the cursor
sets, and re-read through a second `tairix_rt::work::Worker` desk on its own
wait-set token (`MOUNTS_TOKEN`) — submitted, never awaited. A second *desk*,
not a second deferral scheme: the applier's is latest-wins over one job slot,
so sharing it would let an apply and a walk evict each other. `Shell` says it
wants one (`volumes_wanted`) when the pane *comes* on show and the `Run` binary
submits; the answer lands as an ordinary wake. Leaving and returning asks
afresh, because unlike the shipped picture store the mount table moves.

**The three body shapes, and why the shell no longer spells two.** The pane
column was `Option<Form>` + `Option<Gallery>`, matched as a tuple at every
measure/render/scroll/input site, with `(Some(gallery), None)` a state that
could not occur but had to be handled. It is now one `body::Body` enum —
`Statement`, `Form`, `Pictures { form, gallery }`, `Volumes` — so the
impossible pairing is unrepresentable and the storage body got a deliberate
arm at each site rather than falling through a form's. Three predicates carry
what the sites used to re-derive: `composes_controls` (is the column on the
focus ring in its own right), `scrolls_in_pixels` (only a clipped statement
is), and `is_listing`. The plate-stacking arithmetic `Form` carried is
`stack::{place, reveal_from, gap, as_extent}`, shared with the volume cards,
because a second copy of "place plates down a column, seat the ones that fit
whole" is the duplication the charter forbids.

**Scroll steps are in the unit the extent is counted in.** Found while wiring
this: `ScrollModel`'s line and page steps are documented as being in the
model's own scroll unit, but the shell handed every model a 24px/240px step —
including the category strip, counted in *rows*, and a form, counted in
*groups*. One wheel tick over the strip therefore jumped 24 rows, past every
category in the list. A plate- or row-counted column now steps by one of them
and pages by what the column shows; only a pixel-scrolled statement keeps the
pixel step. Regression tests:
`a_wheel_tick_over_the_strip_moves_one_category_row` and
`the_storage_panes_column_scrolls_by_whole_volumes`.

**This must not become a second Storage page.** The Switchboard's System
section already has one, and the two answer different questions — *how full is
each medium* here, *is each volume healthy and how hard is it working* there.
They share facts, and DS5a is where those facts are derived; neither surface
keeps a copy. Two further shared pieces landed with this stage for the same
reason: `lib/procinfo::mount_name_bytes` (the source-else-mount-point naming
rule both surfaces use) and `lib/util::size::{binary_scale, format_at_scale,
format_binary}` (the desktop's prose byte ladder, hoisted out of the
Switchboard's private `format` module so a capacity reads the same on both).
That ladder reaches `EiB`: a byte count is a `u64` throughout the ABI, and a
rung short spells the top of its own domain as four figures of the rung below.

### DS6 — the elevated-apply seam, and General

**Done.** `ElevateRequest::Run` carries a bounded argv — at most
`ELEVATE_MAX_ARGS` (16) arguments, `ELEVATE_MAX_ARG_LEN` (512) bytes each,
`ELEVATE_MAX_ARGV_BYTES` (1024) in all, one admissibility rule shared by the
encoder and the decoder — so a caller can run the tool that already owns a
store with the one change a user asked for instead of that store growing a
second writer. It widens no authority: the request already named an arbitrary
absolute program, and the broker keeps every check it had. A malformed or
over-long vector is refused at the decode, *before* an attempt is spent
against the named account, and the audit records the argument **count** and
never the arguments — the broker hands them over without interpreting them,
so it cannot know which is a secret. The shell's `elevate` builtin grew the
same operands, so the CLI and the GUI reach the writer identically.

**Reading the machine's store.** Settings holds no `CAP_FS_ACCESS` and never
will, so `system.conf` is served by a new ungated `sysinfo-v1` query,
`SYSTEM_CONFIG`, over a kernel introspect domain of the same name: the kernel
already owns the VFS and already parses this document at the root unlock, so
serving it adds no authority to anything and no manifest widens. It answers
the **text**, read fresh (a boot snapshot would report the old value after
`configure` wrote a new one), and `lib/procinfo::system_config` parses it with
`lib/sysconfig` — the engine `configure` writes through. Ungated on the same
ground as `MOUNT_LIST`: a world-readable public document carrying no
credential, with no write path anywhere near it. DS7/DS8's `network.conf` read
is the same shape and lands with its own consumer.

**General.** *About* and *Date & Time* are read-only fact columns — one
label-and-reading row per figure, each from its own ungated query, so one
refusal costs one row and every reading that did not arrive says so. *Login &
startup* and *Caching* are **staged**: a choice edits a working copy, the
pane's action band says how many rows differ, and Apply asks for an account
once and runs `configure` once with every changed key. That made `configure`
accept several `<key> <value>` pairs, applied to one rendered document, so a
group of settings can never be left half written — the defect a run per key
would have had. The master switch's ceiling is shown as the ceiling it is: a
per-class row keeps its own value, because that is what the store says, and
states that caching is off for the machine so it cannot be read as running.
Date & Time launches `datetime.app` through the broker's existing `Launch`,
started and left running.

**The credential question is shared.** The desktop has one credential
surface, `lib/controls::CredentialSheet`: the session's prompt window and this
application's in-window sheet compose the same focus order, wording, "an empty
field is never offered" rule and secret hygiene. It is modal while it is up.

**One more desk each way.** The store read and the elevated run are worker
desks like DS5's, because the broker answers only once the program it started
has exited — a window that waited would stop drawing for the whole of an
authentication and a store write.

### DS7 — Networking: read, and the stack-wide options

Per-interface facts, link state, addresses, and rates from
`NET_INTERFACE_FACTS`/`_STATE`/`_RATES` — the same queries the Switchboard's
Network page reads, through the same client, with no second sampler. The
stack-wide `net.*` sysconfig keys (IPv4/IPv6 enable, IPv6 privacy addresses,
SYN cookies, keepalive, ECN) are staged and applied through DS6. Wi-Fi is a
pane stating §3's absence.

### DS8 — Networking: write, and the store's missing writer

`configure` grows the `lib/netconfig` registry, which that engine's own
contract already names it the writer of and which nothing but the installer
writes today: per-interface kind, match, IPv4/IPv6 method, static addresses,
gateway, DNS servers, MTU, and bond members, over the same closed-key,
fail-closed engine. Settings' Ethernet and DNS panes then stage a change and
apply it by elevating `configure`, which writes the store **and** asks the
network stack to adopt it over its existing `CAP_NET_ADMIN` admin surface, so
a change takes effect without a reboot and without Settings holding
`CAP_NET_ADMIN`. Devmgr's static-only note (`netcfg.rs`) is retired in the same
stage: the runtime-reload increment it defers is this one.

### DS9 — Users & Groups

**The read.** The whole `users_admin` syscall is gated on `CAP_USER_ADMIN`, and
the only other account read — `users_db_read` under `CAP_USERS_READ` — answers
the credential database itself. Settings takes neither.

`USER_DIRECTORY` answers uid and username alone, which is not a Users pane. The
pane is therefore built from three reads of *different* authority, rather than
from one ungated query widened until it is enough:

- **The roster** — every account's uid and username — is the existing ungated
  `USER_DIRECTORY`, walked paged. A sibling ungated `GROUP_DIRECTORY` answers
  gid and group name on the same ground and nothing else: rendering a gid is
  the same display need as rendering a uid.
- **The caller's own account** — full name, shell, home, primary gid, group
  memberships — is read for the uid the kernel attests, ungated, because a
  principal reading its own record crosses no boundary.
- **Any other account's fields, every account's lock state, and the grant
  ceiling** are answered by the administrator-authenticated run the write path
  already performs, and are shown only after it. This needs no new interface:
  `users_admin` already carries `ListUsers` and `ListGroups` behind
  `CAP_USER_ADMIN`, so the gated half of the pane is a read the elevated tool
  can already serve.

The third line is the one to be explicit about, because widening the directory
is the tempting shortcut and it is a real loss. An ungated lock state is an
enumeration of which accounts are live and so worth attacking; an ungated shell
and home path are reconnaissance any unprivileged process — a compromised
parser sandbox included — learns nothing of today. None of them is a display
pairing, so none rides the directory's justification, and TAIRiX has no
world-readable `passwd` file to inherit the habit from. The default is closed: a
field enters the ungated directory only where the DS9 review positively shows a
name cannot be rendered without it, and the burden is on the field. Password
records stay behind `CAP_USERS_READ`, the grant ceiling behind
`CAP_USER_ADMIN` — it is a map of the machine's authority, not directory data —
and no new capability is added for any of this.

Both directory frames are fixed-width, fail closed on decode, and enter the
`lib/abi` fuzz seed like every other frame; the roster is walked paged, so a
machine with many thousands of accounts leaves no whole-set copy resident.

**The write.** The user's own full name and password are changed through the
broker's re-authentication of that same account. Administering another account
— create, modify, lock/unlock, delete, set grants, groups — elevates the tool
that owns the syscall, which means the user-admin command family grows the
operations `users_admin` already carries and no tool yet spells (modify,
delete, lock/unlock, set grants, set password, delete group). Settings
reimplements none of them, and holds no path to any of them without a password.
A grant the authenticated account may not confer is refused by the kernel and
stated; the pane never pre-approves an escalation, and the kernel's
never-widen and last-administrator rules remain the only arbiters.

### DS10 — Notifications

The session owns the notification feed the taskbar draws, so it owns the
policy: a per-source allow/deny and a minimum severity, plus the desktop-wide
"show none" switch, in DS3's document. The session enforces it where it
already receives a `NotifyRequest` — one gate, at the one intake — so a
suppressed notification is never delivered, drawn, or logged as shown. Sources
are named by their attested bundle identity, never by anything a sender says
about itself, and a source that has never notified is not listed (an empty
list reads as *none*, which is the truth).

### DS11 — Keyboard and Mouse: the session's input policy

The session routes every pointer and key event, so it is the owner: DS3's
document grows `pointer.primary` (left/right), `pointer.double_click_ms`,
`pointer.speed`, `key.repeat_delay_ms`, and `key.repeat_rate`. Button mapping
and key repeat are applied where the session already resolves the event, so no
app sees the unmapped form. The **double-click interval** is the one that needs
care: apps resolve their own double-click today (the file manager's activation
gesture does), which is two intervals waiting to disagree. The session
therefore *publishes* it alongside the scale and appearance the window channel
already carries in `DesktopInfo`, and the file manager's private constant is
deleted and read from there in this stage — one interval for the desktop, set
once. The document spells the intervals in milliseconds, because a file a human
edits should; every ABI-visible and in-memory form of them — the `DesktopInfo`
field included — is `Duration64`, like every other span `lib/abi` carries, and
`lib/abi` has no millisecond field to copy. Cursor set and size come from DS3.
Layout, modifier remap, and shortcuts state §3's absence and name what each
needs.

### DS12 — Lock Screen and Screensaver: the idle interface

The one new subsystem this plan builds. The session gains a single idle
deadline: the `Time64` timestamp of the last input event, and one timer armed
**only** while a policy has a deadline pending — never a tick, never a poll, so
an idle desktop still wakes no core it did not have to. Two policies ride it,
both in DS3's document: blank-or-screensaver after *M* minutes, and lock after
*N*.
The Screensaver pane offers what the desktop can actually draw — blank, the
desktop backdrop dimmed, and the wallpaper slideshow the chooser's catalog
already enumerates — and nothing it cannot. The Lock Screen pane sets the
lock deadline, states plainly that unlocking always requires this account's
password (it is not a setting, and pretending it were would be a security
lie), and offers **Lock Now**, which is the session's existing lock, not a
second one.

### DS13 — the QEMU vertical, and docs

A dedicated `settings_qemu_aarch64` vertical, a short sibling of the autoload
desktop vertical rather than a further stage on it (so a gate mis-count in one
choreography cannot wedge the other). It boots the autoload root disk, unlocks,
logs in, starts `desktop`, opens the system quick-actions menu, chooses
*Settings…*, and then: screendumps the shell on General; walks the sidebar to
Appearance and flips the desktop to light, witnessing the change in the *next*
dump of the desktop behind the window; walks to Storage and dumps the capacity
tracks; and walks to a §3 absence pane and dumps its statement. PASS needs the
guest's own witnesses — an `APP_LOADED` naming the settings bundle, the window
creates served on the reserved endpoint, and the session's witness that each
frame is on screen before the runner reads it back.

Docs in the same stage: `docs/src/desktop/settings.md` grows each pane's own
content as DS3–DS12 land it, and `docs/src/userland/confd.md` gains whatever
scope keys DS3's document adds. The page itself, its `SUMMARY.md` entry, the
`Help/` topic, and the `docs/src/desktop/taskbar.md`,
`docs/src/desktop/widgets.md`, `docs/src/desktop/icons.md` and
`docs/src/lib/controls.md` updates landed with DS2.

### DS14 — retire the second form idiom

The file manager's hand-rolled permissions grid (`lib/browse`'s `PermGrid`) and
`datetime.app`'s six-field row are rebuilt on `FieldGroup`/`FieldRow`, and the
private layout arithmetic each carries is deleted. Leaving three form idioms in
one desktop after DS1 is exactly the duplication `AGENTS.md` §2.2 forbids, and
the conversion is what proves the family is genuinely general rather than shaped
around one app. Their existing tests are retargeted, not
weakened, and the QEMU verticals that dump those surfaces re-baselined.

---

## 6. Sequencing and dependencies

The graph is the ledger's `Depends on` column; this is the reasoning behind the
four edges that are not obvious from it.

**DS1 does not land alone.** It is host-provable by itself, but a shared family
whose only caller is its own gallery tab is one caller, not the two independent
ones `AGENTS.md` §23.3 requires of a shared helper. So it lands with the first
of DS14's two conversions in the same increment — `datetime.app`'s six-field
row is the smaller, and its private arithmetic dies with it — and the family is
proven by a surface that already had a form to draw rather than by a gallery
shaped around it.

**DS2 is independently useful the day it lands**, which is why it comes before
any writing pane: every category is reachable and every absence is honest.

**DS3 is the template, not merely the first pane.** It establishes the
user-scope document and the session's apply policy, so DS4, DS10, DS11 and
DS12 are all further keys in that one document and may land in any order once
it has.

**Shared machinery lands with its consumer.** DS6's argv extension and DS8's
`configure` extension are each in the same increment as the pane that uses
them, so nothing speculative is added ahead of a caller (`AGENTS.md` §2.4).

**DS5a is the exception that proves that edge, and is not speculative.** It
moves a derivation that already had a caller rather than adding one for a
caller to come: every item it hoists was live in the Switchboard, `sysmon`,
`fstree`, `stress` or `df` the day it landed, and each of those now reads the
shared definition. DS5 becomes its second consumer. It is a separate
increment only because a half-moved derivation with two callers is worse than
either end state.

## 7. What this explicitly refuses to become

To stay first-class and bloat-free, Settings will **not** grow: a privileged
settings daemon holding the union of every domain capability; a second writer
for any store that already has one; a plug-in or extension surface for
third-party panes (a closed registry is what makes the authority map
auditable); a wizard or "assistant" flow; a scripting or automation surface; a
profile/sync mechanism; a second theming, rendering, or control path; a
duplicate of the Switchboard's monitoring; a second route to a destructive
machine transition; or a control that changes nothing so that a category can
look complete. A domain that belongs to another subsystem is *reached*, never
reimplemented here — and a domain that does not exist yet is *stated*, never
mimed.
