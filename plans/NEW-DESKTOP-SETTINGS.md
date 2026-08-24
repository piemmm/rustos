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

## Status

`in progress` — DS1 is `done`, DS2–DS14 remain. The form family every pane
composes is `lib/controls::form` (`FieldRow`, `FieldGroup`, `FieldControl`,
`FieldLayout`), specified in `plans/GUI-CONTROLS-DESIGN.md` §11.40; the row
chrome it shares with `ListRow`/`TableRow` lives in that crate's `paint` core,
and a control's own measured width comes from `Button::measured_width` /
`ComboBox::measured_width`. Nothing of the Settings application itself exists
yet: DS2 is the first stage that puts a window on screen.

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
  Settings' Power pane does not offer a second route to them (§2.2, and
  macOS parity: shutting down is not a settings pane there either).

  Concretely, the one reader is `lib/procinfo`: it already owns the paged
  walks over `MOUNT_LIST`, `USER_DIRECTORY`, the process list, the CPU-time
  stats and the pressure fetch, and both the Switchboard and the CLI tools
  read through it. Settings adds no sampler of its own; where it needs a
  derivation the Switchboard also needs — a mount record turned into a
  capacity reading — the derivation moves into `lib/procinfo` and both read
  it, rather than each keeping a private copy that will drift.

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
  An application that could change everything is precisely the ambient-
  authority god-app §4 and §5.2 forbid, and a settings *browser* does not need
  to be one: every change is either a request to the process that already owns
  that domain, or a re-authenticated run of the tool that already writes that
  store. §2 is the authority map, pane by pane. A pane whose write path is
  refused states the refusal and changes nothing (§2.24) — it never reports a
  success it did not get.

- **Three write paths, and no fourth.** Every settable in this plan reaches
  one of exactly three owners:

  1. **User scope → the desktop session.** Appearance, contrast, density,
     reduced motion, UI scale, cursor set, wallpaper and pinboard keys,
     notification policy, pointer and idle policy. Settings renders the
     document and posts it to the session, which validates it, applies it, and
     persists it to its own published app-data scope. This is the
     `plans/PINBOARD.md` §6 rendezvous exactly as the wallpaper chooser
     already uses it: the session is the only writer, an application publishes
     only its own scope, and the desktop adopts a change **only after the
     write succeeded**, so memory and disk cannot diverge.
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
  hand-rolled a permissions grid, the wallpaper chooser a column of four
  drop-downs, and `datetime.app` a row of six fields. `lib/controls::form` is
  the one family (DS1), and DS4/DS14 rebuild those three surfaces on it,
  because two form idioms in one desktop is the duplication §2.2 forbids. The
  family composes the existing row chrome and control families; it
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
  pane and scrolls that row into view; this is the one thing macOS's settings
  search does well and it is cheap here because the registry already knows
  every label.
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
| General → About | `SYSTEM_IDENTITY`, `UPTIME`, `CPU_INFO`, `KERNEL_MEMORY_STATS` | — (read-only) | reading renders unmeasured |
| General → Login & startup | `lib/sysconfig` `os.loginType` | elevated `configure` | Authority Mark, value unchanged |
| General → Caching | `lib/sysconfig` `cache.*` | elevated `configure` | Authority Mark, value unchanged |
| General → Date & Time | `WallClockReading` | elevated `datetime.app` (launched) | prompt not shown, clock untouched |
| Appearance | active `Theme` | session apply | apply refused, stated in footer |
| Wallpaper | session's published pinboard document | session apply; gallery → `wallpaper.app` | apply refused, stated |
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
| Accessibility | active `Theme` (contrast, density, motion), scale, cursor size | session apply | apply refused, stated |
| Language & Region | the bundle `Help/` locale set, `lib/sysconfig` | elevated `configure`; zones → `plans/TIMEZONES.md` | Authority Mark |
| Sharing | — | — | pane states absence (§3) |
| Users & Groups | ungated `USER_DIRECTORY` / `GROUP_DIRECTORY` (no credential material) | elevated user-admin tool (DS9) | Authority Mark, account unchanged |
| Storage | `MOUNT_LIST` + each volume's `VolumeStats`, `VOLUME_IO_HEALTH` | — (read-only; mounting is the file manager's) | reading renders unmeasured |

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
| Sound | no audio subsystem at all: no codec driver, no mixer, no stream API, no `CAP_AUDIO` | a new `plans/AUDIO.md`: driver, mixer service, `lib/abi` stream + volume vocabulary |
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

That is the whole addition. Everything else a pane needs already exists:
`Toggle`, `Checkbox`, `Radio`, `ComboBox`, `Slider`, `TextField`, `Button`,
`Tabs` (the sidebar), `SearchField`, `Breadcrumb`, `ScrollBar`, `Menu`,
`Dialog`, `FactList` (read-only panes), `MetricTile` (Storage' capacity
tracks), `StatusPill` (a link state), and `ActionRail` where a pane commands a
selected subject. **No new control is added for a job an existing one does.**

New `IconKind` glyphs, one per category, each with the mandatory first-party
built-in vector glyph so the sidebar can never blank: `Settings`, `Appearance`,
`Wallpaper`, `Display`, `LockScreen`, `Screensaver`, `Power`, `Bluetooth`,
`Sound`, `Notifications`, `Keyboard`, `Mouse`, `Trackpad`, `Touchscreen`,
`Printer`, `Accessibility`, `Language`, `Sharing`, `Users`, `Storage`.
(`Network` and `Volume` already exist and are reused, not duplicated.)

---

## 5. Stages

Each stage is one fully-gated increment: it lands with its host tests, its
rustdoc and `docs/` page, and a green whole-workspace validation gate
(`cargo xtask ci`), and — where the behaviour is only observable end-to-end —
extends the QEMU vertical rather than a faked run. A stage that turns out
larger than one clean increment is split and staged here, never shipped
half-done.

### DS1 — `lib/controls::form`: the form-field family — `done`

`FieldRow` and `FieldGroup` per §4, over the existing row chrome, plate,
metrics, and state vocabulary, with a `widgets.app` gallery tab. What a later
stage may rely on, and must not re-derive:

- **Room is given out control, label, description.** The slot is reserved
  first, the label elides into what remains, and the description draws only
  while the label fits whole. A slot never exceeds half the row's content
  (`form::slot_ceiling`), so a label always has room to be read.
- **A row's authority is the setting's.** `FieldRow::set_state` shares
  enablement and authority with the control in the slot, so a denied row cannot
  hold an actionable control. A pane therefore states a refusal by setting the
  *row*, never by remembering to set two states in step.
- **The owner places the choice popup.** `FieldGroup::popup_anchor` names the
  row and slot to anchor an expanded `ComboBox` list to; the owner places it,
  hands it back through `FieldLayout::with_popup`, and paints it with
  `render_popup` after every group. The pane (DS2) owns the placement rule,
  because only it knows the viewport the list has to fit in — and DS2 is where
  the one `ComboBox` placement rule is hoisted, retiring the three private
  copies (the widget gallery, the wallpaper chooser, the Switchboard's task
  grouping) rather than adding a fourth.

### DS2 — the Settings shell, every category reachable, nothing faked

The bundle: `userland/apps/settings` (`tairix-settings`), `AppInfo.toml` with
`id = "os.tairix.settings"`, `kind = "application"`, `library = "SystemTools"`,
`purpose`, `author`, `capabilities = ["CAP_CONSOLE_WRITE", "CAP_SHM"]`, its
own SVG icon in `Resources/`, a `Help/en-US/` tree, and a `README.md`.
`build.rs` mirrors the sibling apps' `freestanding` cfg so the library target
is host-testable and `Run` is a freestanding program.

The shell: the closed `Category`/`Pane` enums and the one ordered `CATEGORIES`
table; `resolve_frame`; the vertical `Tabs` sidebar with category glyphs and
in-place pane expansion; the `SearchField` and the index derived from the
table; the `Breadcrumb` band; the scroll model; the focus-region policy and
cursor; resizable window with frame re-map on `Resized`; and the one
absence-pane renderer that draws a §3 row's statement. Every category in §2 is
listed and selectable from this stage on — the ones with no backing state their
absence, the ones with backing land their content in DS3–DS12.

The **launch row**: `userland/gui/taskbar/src/system.rs` grows
`SystemAction::Settings` with label `Settings…`, a `SETTINGS_BUNDLE`
identifier, and a `settings_installed` permit resolved against the catalog the
session handed the bar — exactly as `TaskShell` already is. It maps to the
bar's existing `TaskbarResponse::LibraryLaunch`, so the session gains **no new
launch path** for it (§2.2), and the row renders non-actionable with
`REASON_NOT_INSTALLED` when the bundle is absent. It sits at the head of the
appearance group, above *Light Appearance*, because it is the general form of
the two rows beneath it.

Host tests: the registry's totality (every `Pane` has a row and every row a
renderer); the search index covering every declared label; frame shedding at
the narrow width with the content column surviving; the cursor reaching every
row; the taskbar row's presence, order, permit, and command mapping.

### DS3 — Appearance and Accessibility: the session's user-scope document

The first *writing* panes, and the template for every other user-scope write.
The desktop settings document (today `lib/wallpaper`'s five pinboard keys)
grows the desktop's own appearance registry — `appearance`, `contrast`,
`density`, `motion`, `scale`, `cursor.set`, `cursor.size` — with the same
closed-key, tolerant-read, canonical-render discipline and the same one engine.
The session applies each through the paths it already owns
(`DesktopShell::set_scale`, the theme registry's `set_appearance`, the cursor
registry's `set_active`) and persists the whole document to its published
scope; Settings posts and re-reads. Appearance and Accessibility are two views
of one registry — contrast, density, motion, scale, and cursor size appear in
both, from one definition, because a reader looks for them in either place.

Host tests: the registry round trip; a refused value leaving exactly its own
key at the default and naming it; the session's apply policy refusing an
unattested caller and a malformed document; every immediate row's commit
reaching the document it claims to.

### DS4 — Wallpaper

The pinboard keys as `FieldRow`s (fit, backdrop, icon flow, sort) applied
immediately through DS3's channel, the current wallpaper named with its
resolved path, and a **Choose Picture…** button that launches
`wallpaper.app` for the gallery. The gallery stays in the chooser deliberately:
decoding an untrusted image needs the sandbox worker the chooser spawns, and
Settings must not acquire `CAP_PROC_SPAWN` to grow a second gallery. In the
same stage the chooser's own four drop-downs are rebuilt on `FieldGroup`/
`FieldRow` so the two surfaces share one form idiom.

### DS5 — Storage

The per-medium used-space overview: one `FieldGroup` per mount walked from
`MOUNT_LIST` through `lib/procinfo::for_each_mount`, each with its volume
label, filesystem, device, mount point, a `MetricTile` capacity track over the
volume's own `VolumeStats` block counts, and its `VOLUME_IO_HEALTH` state as a
`StatusPill`. Read-only: mounting and unmounting are the file manager's and
`mount`'s, and a second route to them is duplication. A volume whose stats or
health could not be read renders unmeasured, never a full bar or a green pill.

**This must not become a second Storage page.** The Switchboard's System
section already has one, and the two answer different questions — *how full is
each medium* here, *is each volume healthy and how hard is it working* there —
but they share a fact, so they must share its derivation. The mount record →
capacity/health view model moves out of the Switchboard's private
`view/system_data.rs` into `lib/procinfo` in this stage, and both surfaces are
rebuilt on it. If that conversion turns out to be more than one clean
increment, it is split out and staged before DS5 rather than shipped as a
second copy.

### DS6 — the elevated-apply seam, and General

`ElevateRequest::Run` gains a **bounded argv** (a count bound, a per-argument
length bound, and a total bound, fixed-width and fuzzed like every other
`lib/abi` frame). Today the broker can only start a whole interactive program,
which is why every privileged desktop action has had to become its own app;
with argv it can run the tool that already owns a store with the one change
the user asked for. This widens no authority — the request already named an
arbitrary absolute program — and the broker keeps every existing check: it
authenticates the named account, loads through the ordinary signed load gate,
runs as that account, and audits the decision.

Its first consumer is General: **About** (identity, OS version, uptime, CPU and
memory facts as a `FactList`), **Login & startup** (`os.loginType`), and
**Caching** (the five `cache.*` keys with their `auto`/`off` sets and the
master switch's ceiling shown as the ceiling it is), each staged and applied by
elevating `configure`. **Date & Time** shows the current reading and launches
`datetime.app` through the broker's existing `Launch`, unchanged.

Host tests: the argv codec (bounds, fail-closed decode, no panic, fuzz seed);
the broker refusing an over-long or malformed argv before authenticating
anything; Settings' staged-pane model (working copy, dirty rows, revert,
outcome reporting) against an injected elevation seam.

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
the credential database itself. Settings takes neither. It reads the ungated
`USER_DIRECTORY` query, which exists for exactly this purpose and carries no
credential material, and identifies the user's own account by the uid the
kernel attests for it.

That query answers uid and username alone today, which is not a Users pane. So
this stage extends the **directory** rather than reaching for a capability: the
`UserDirectoryRecord` grows the non-secret display fields the database already
holds (full name, shell, home, primary gid, lock state) and a sibling
`GROUP_DIRECTORY` query answers groups and their members. Both stay ungated for
the reason `USER_DIRECTORY` already is: this is the display pairing every
`ls -l`, `ps` and `top` needs to render a name instead of a number, it carries
no credential material and no per-principal secret, and the added fields are of
exactly that class. The password records stay exactly where they are, behind
`CAP_USERS_READ`, and the extension is reviewed field by field against that
line. The capability
**grant ceiling** is deliberately *not* directory data: it is a map of the
machine's authority, so it stays behind `CAP_USER_ADMIN` and is shown only
after an administrator has authenticated.

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
once. Cursor set and size come from DS3. Layout, modifier remap, and shortcuts
state §3's absence and name what each needs.

### DS12 — Lock Screen and Screensaver: the idle interface

The one new subsystem this plan builds. The session gains a single idle
deadline: the timestamp of the last input event, and one timer armed **only**
while a policy has a deadline pending — never a tick, never a poll, so an idle
desktop still wakes no core it did not have to. Two policies ride it, both in
DS3's document: blank-or-screensaver after *M* minutes, and lock after *N*.
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

Docs in the same stage: `docs/src/desktop/settings.md` (the surface, the
authority map, the absence table, and the three write paths), its `SUMMARY.md`
entry, the `Help/en-US/settings.md` topic, and updates to
`docs/src/desktop/apps.md`, `docs/src/desktop/taskbar.md` (the new row),
`docs/src/desktop/widgets.md` (the form family's gallery tab), and
`docs/src/userland/confd.md` if DS3's document lands new scope keys there.

### DS14 — retire the second form idiom

The file manager's hand-rolled permissions grid (`lib/browse`'s `PermGrid`) and
`datetime.app`'s six-field row are rebuilt on `FieldGroup`/`FieldRow`, and the
private layout arithmetic each carries is deleted. This is not polish: leaving
three form idioms in one desktop after DS1 is exactly the duplication §2.2
forbids, and the conversion is what proves the family is genuinely general
rather than shaped around one app. Their existing tests are retargeted, not
weakened, and the QEMU verticals that dump those surfaces re-baselined.

---

## 6. Sequencing and dependencies

DS1 is the only true prerequisite for everything after it, and it is
host-provable on its own. DS2 lands the shell and the launch row over DS1 and is
independently useful the day it lands: every category is reachable and every
absence is honest, which is already better than a settings app that hides what
it cannot do. DS3 establishes the user-scope document and the session's apply
policy, and DS4, DS10, DS11 and DS12 are all further keys in that one document
— they can land in any order once DS3 has. DS5 depends only on DS2 (it is
read-only) and can land beside DS3. DS6 gates every machine-scope write, so
DS7's options, DS8, and DS9 all follow it; DS7's *readings* need only DS2 and
may land earlier. DS8 depends on DS7 (the panes it writes to) and on the
`configure` extension it brings. DS12 depends on DS3 and on the session's
existing lock. DS13 closes the core; DS14 pays off DS1's debt and may land any
time after DS4.

The two stages that touch shared machinery — DS6's argv extension and DS8's
`configure` extension — each land with their consumer in the same increment, so
nothing speculative is added ahead of a caller (§2.4).

---

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
