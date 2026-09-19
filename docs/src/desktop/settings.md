# Settings

`settings.app` is the windowed application that configures the desktop and the
machine, opened from the *Settings…* row of the Switchboard capsule's system
quick-actions menu ([the taskbar](taskbar.md#the-system-quick-actions-menu)),
from the desktop's Program Library, or by name from a shell. The staged build
plan is `plans/NEW-DESKTOP-SETTINGS.md`; this page describes what is on the
tree.

## What it is, and what it deliberately is not

**The Switchboard observes; Settings changes.** The
[Switchboard](switchboard.md) reports what the machine *is doing* — tasks,
pressure, faults, per-core load, each volume's health — and its commands act
on running things. Settings changes what the machine and the desktop *are
configured to be*, and holds no command that acts on a running task. Where
both want the same fact, the fact has one reader; where both would want the
same command, only one has it, which is why the machine's power transitions
stay in the system quick-actions menu and Settings offers no second route to
them.

**Settings holds no domain authority, and never will.** Its manifest requests
`CAP_CONSOLE_WRITE` and `CAP_SHM` and nothing else — the same class as the
[widget gallery](widgets.md). It holds no `CAP_TIME_SET`, no `CAP_NET_ADMIN`,
no `CAP_USER_ADMIN`, no `CAP_SYSTEM_POWER`, no `CAP_STORAGE_ADMIN`, no
`CAP_DISPLAY`, no `CAP_FS_MOUNT`, and no `CAP_FS_ACCESS`. It does not hold
`CAP_USERS_READ` either, which is the sizing decision worth naming: that
capability gates a read of the whole credential database *including every
password record*, so a settings browser that wanted to print a user's full
name would be holding every hash on the machine.

An application holding the union of every domain's authority would be exactly
the ambient-authority god-app the charter forbids (`AGENTS.md` §4, §5.2), and
a settings *browser* need be no such thing: every change is either a request
to the process that already owns that domain, or a re-authenticated run of the
tool that already writes that store. Nothing here can be tricked into an
escalation, because there is no capability in it to escalate with.

**One instance, and no icon-bar slot.** Settings is part of the desktop
rather than an application the user manages. Its signed manifest declares
`icon-bar = false`, so it has no slot of its own and **closing its window
ends the program** — there is no slot left holding a handle on a windowless
process. It is a singleton, which is the manifest's own default: relaunching
it while it is open raises the window that is already there rather than
starting a second view of one machine's configuration, each able to overwrite
the other's applies. The desktop's one launch funnel resolves that, so every
route in — the system menu, the Program Library, a shell — behaves the same.

**Three write paths, and no fourth.** Every settable reaches one of exactly
three owners: the desktop session, for the user's own desktop; the tool that
already writes a machine-wide store, run as a re-authenticated account; or the
syscall's own tool, likewise. A pane whose write path is refused states the
refusal and changes nothing — it never reports a success it did not get.

## The surface

```text
 ┌──────────────────────────────────────────────────────────────────┐
 │ [search field]      │  Settings › Networking › Ethernet          │  band
 ├─────────────────────┼────────────────────────────────────────────┤
 │ ⚙ General         ▾ │                                            │
 │     About           │  the pane on show                          │
 │     Caching         │                                            │
 │ ◑ Appearance        │                                            │
 │ ⇅ Networking        │                                            │
 │ …                   │                                            │
 └─────────────────────┴────────────────────────────────────────────┘
```

- **The sidebar** is `tabs::Tabs` in its vertical, sidebar-list form — the
  control the Switchboard's System section already uses, turned on its side,
  not a second selection model. Each row carries its category's `IconKind`
  glyph and its label; a category holding more than one pane carries a
  disclosure chevron and its panes appear as nested rows of the same strip, so
  one cursor walks the whole column.
- **The search field** sits above the sidebar and filters the strip to the
  categories and panes a word reaches — by a category's label, a pane's title,
  or a setting label a pane declares. The index is derived from the one
  registry table, so a searchable setting cannot exist without a row that
  shows it.
- **The location band** carries a `nav::Breadcrumb` reading
  `Settings › <category> › <pane>`. A category holding one pane shares its
  name, so the trail shows two crumbs rather than saying the same word twice.
- **Region shedding.** One resolver (`resolve_frame`) divides the client once
  per layout, and the paint and the hit test both read it, so a press can
  never land on a control drawn elsewhere. A client too narrow to seat the
  sidebar sheds it — and the search field with it, there being no strip left
  to filter — and the leading crumb then lists the categories as a `Menu`. The
  content column always survives, because the pane is what the reader came
  for.
- **The cursor.** Tab cycles the search field, the trail, the sidebar, and the
  pane column; a region the frame did not seat is not on the ring, so Tab
  never lands somewhere the reader cannot see. Within the sidebar, Up and Down
  walk every row — category and pane alike — and Enter opens it.

## The registry is the surface

`Category` and `Pane` are closed sets and one ordered `CATEGORIES` table is
the single definition of the sidebar, the search index, the location trail,
the keyboard cursor and the pane dispatch. A pane cannot exist without a row,
or a row without a pane — the crate's own tests hold both directions — so
adding a category is adding a row and a renderer, never editing the shell.

## Appearance and Accessibility

Two of the three panes that compose real controls today. They are two views
of one registry: light/dark is Appearance's alone, and contrast, density,
motion and the interface scale appear in both — from one definition, because
a reader looks for them in either place.

| Setting | What it changes |
|---|---|
| Appearance | Light or dark. |
| Contrast | Normal, high, or monochrome — monochrome tells every state apart by shape rather than by colour. |
| Density | Compact, normal, or comfortable. It moves the three metrics that decide how much room a control is given and nothing else, so a compact desktop packs the same controls closer rather than drawing different ones. |
| Motion | Full, or reduced — a reduced state change is still visible, it just happens at once. |
| Interface scale | How large every desktop length is drawn. |
| Pointer set | Which cursor artwork the pointer is drawn from (Accessibility's alone). |
| Pointer size | How large the pointer is drawn, on top of the interface scale (Accessibility's alone). |

Each row commits on the choice: the change is cheap, reversible, and its
effect is the feedback, so there is no Apply button to go stale. The pane
renders **only the keys it edits** and posts them to the desktop session,
which merges them over what it already holds — a wallpaper change and an
appearance change cannot undo each other.

The round trip runs on a worker, never on the window's event loop: the
session answers only once its own publisher has written the store, so waiting
for it inline would freeze this window for a disk commit. The rows show the
reader's choice at once and adopt the *durable* value when the answer lands,
so a refusal states its reason and puts the row back rather than leaving a
value on screen the next login would not restore.

Accessibility additionally carries the **pointer pair**, in a POINTER group
of its own. *Pointer size* is a closed ladder over the desktop's
`cursor.size` setting, magnifying the pointer's logical side on top of the
interface scale. *Pointer set* is the cursor artwork, and its choice space
is the one thing on these panes the settings document cannot supply: which
sets exist is what the read-only store holds, and Settings may not read it.
So the session lists the store once at its own bring-up and answers a
capability-free window-channel query (`QueryCursorSets`) — the whole choice
space in one reply, since it is bounded small — exactly as it serves the
wallpaper catalog. The built-in `Standard` set is always offered beside
whatever the store carries, and a set the document names that the store no
longer holds is still offered under its own name, so opening the pane never
quietly changes the pointer someone chose. [The cursors
page](./cursors.md) has the store's layout and the artwork pipeline.

## Wallpaper

The third composed pane, and the desktop picture's only home: the backdrop
menu's `Change Background…` opens Settings here rather than a second
application. Its four rows — fit, backdrop, icon arrangement, icon sort —
come from the same one registry as Appearance's, and post the *pinboard*
half of the desktop's document, so a picture change and an appearance change
cannot undo each other.

Beneath them is a gallery of the shipped pictures, and Settings holds no
authority over any of it. Listing the store needs a filesystem capability
and decoding a picture needs a parser sandbox; this application requests
neither, so the desktop session serves both — it answers a catalog page from
the listing it took at its own bring-up, and renders one candidate at a time
into a shared-memory region Settings created and granted. A render names a
catalog position rather than a path, so it cannot be used to make the
session read a file the caller chose.

Every tile is requested and never awaited: a paint draws the pictures that
have come back and a built-in glyph for those that have not, so the pane is
usable from its first frame. A picture the desktop refuses is not asked for
again. [The pinboard's page](./pinboard.md) has the whole arrangement.

## Absence is stated, never mimed

A control that would change nothing is never drawn. Each pane declares what
backs it, and the three answers are different facts to a reader:

| Backing | What the pane says |
|---|---|
| it composes real controls | nothing — the rows are what it says |
| nothing in this system can serve it | what is missing, and what would have to exist |
| the readings and writes exist, and this surface does not yet compose them | what the pane will show, and where the setting is read or set today |

The two stated absences draw through one renderer, quiet and on the surface
behind them with no plate — the same shape every other stated absence in the
desktop takes, because a plate would read as something to interact with.

Seven of the categories a desktop should offer have no subsystem beneath them
on this tree at all: there is no audio stack, no Bluetooth stack, no
print or scan stack, no touchpad or touch driver, no 802.11 driver, and no
file- or screen-sharing server. Settings cannot invent them, and it must not
draw a volume slider that changes nothing. So those categories are present,
reachable, and honest: each states what is missing and what would have to
land.

## Authority map

One row per pane: what backs its readings, where a change goes, and what a
refusal looks like. `plans/NEW-DESKTOP-SETTINGS.md` §2 is the full table and
the staged source of truth; the shape of it is:

- **read-only panes** (About, Storage) render a reading, or render *unmeasured*
  when the reading could not be taken — never a fabricated zero;
- **user-scope panes** (Appearance, Wallpaper, Lock Screen, Screensaver,
  Notifications, Keyboard, Mouse, Accessibility) post a document to the
  desktop session, which validates it, applies it and persists it to its own
  published app-data scope; the desktop adopts a change only after the write
  succeeded, so memory and disk cannot diverge;
- **machine-scope panes** (Login & startup, Caching, TCP/IP, Ethernet, DNS,
  Language & Region) ask the console's elevation broker to re-authenticate an
  account that may, and run the same `configure` program the command line
  uses, so the CLI and the GUI are literally the same writer;
- **kernel-scope panes** (Date & Time, Users & Groups) elevate the tool that
  owns the syscall, never acquiring the capability here.
