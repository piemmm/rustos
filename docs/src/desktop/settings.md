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
  not a second selection model. Each row carries its category's colour badge
  (a white symbol on the category's hue, [Desktop icons](icons.md)) at the
  theme's sidebar icon size, and its label; a category holding more than one
  pane carries a disclosure chevron and its panes appear as nested rows of the
  same strip, so one cursor walks the whole column. The badges are compiled
  in and retained in the window's own icon cache, once per side, so the strip
  never rasterises one per frame, and the cache gives its pixels back on the
  memory-pressure wake.
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

**The window is titled with the pane on show**, as a file manager's window is
titled with its folder, so the title bar and the icon bar's window list say
where it is. It retitles only after presenting the pane, so the title never
names a frame the screen is not showing yet — which is also what lets the
desktop session's witness for a new title on screen stand for the pane's
frame.

## General

Four panes, two kinds.

- **About** and **Date & Time** are read-only fact columns: one label and
  reading per figure, each from its own ungated query, so one refusal costs
  one row and every reading that did not arrive says so. About states the
  machine's name and id, the OS version, uptime, processors and memory; Date
  & Time states the wall clock and the source it was set from, and its band
  starts `datetime.app` as an authenticated account, leaving it running — the
  clock is that application's to set.
- **Login & startup** (whether the machine starts at a text login or a
  graphical one) and **Caching** (how much memory each cache class may keep,
  under one master switch) are **staged** over the machine's `system.conf`,
  read through the ungated `SYSTEM_CONFIG` query and parsed by the
  `lib/sysconfig` engine `configure` writes through. Apply asks for an
  account once and runs `configure` once with every changed key (see the
  authority map below). The master switch's ceiling is stated on the rows it
  takes away rather than rewriting their values, because what the store holds
  is what would apply if the switch came back on.

## Appearance and Accessibility

Two views of one registry: light/dark is Appearance's alone, and contrast,
density, motion and the interface scale appear in both — from one definition,
because a reader looks for them in either place.

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

The desktop picture's only home: the backdrop
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

## Storage

The one pane that composes no settable at all: there is no table behind it,
just the volumes the machine turns out to have. One card each, in the mount
table's own order:

| What the card shows | Where it comes from |
|---|---|
| the volume's name, as its caption | its backing source, or its mount point where the table gives it no source — the same naming rule the Switchboard's device rail uses |
| a health capsule on that caption line | `MountAvailability`, banded to `VolumeHealth` and toned through the one band→role binding |
| mount point, filesystem, device, medium, availability | the mount record's own fields |
| a capacity card with a track | `VolumeBytes`: how much of the *whole medium* is gone, the byte pair, and what is still available |

Read-only throughout. Mounting and unmounting are the file manager's and
`mount`'s, and a second route to them here would be two ways to do one thing;
the pane composes no settable, so the column's keyboard is its scrollbar's.

**Nothing is derived here.** Every figure comes from the one shared volume
view model in `lib/procinfo`, which `df`, `sysmon` and the
[Switchboard](switchboard.md) read too, so a disk cannot be half full on one
surface and nearly full on another. The two shares stay distinct: the track is
of the whole medium, which is what a capacity bar means, while `df`'s GNU
`Use%` divides by what a caller may actually allocate and so reads higher on a
format that withholds a reserve.

**A volume that reports no capacity gets no bar.** The in-RAM layout mounts
report an all-zero accounting; their card says the format tracks no fixed
capacity rather than drawing a full bar or an invented percentage.

**The pane needs no capability the bundle does not already hold.** `MOUNT_LIST`
is ungated — the mount table is system-wide and secret-free, and `df` reads it
the same way. The per-device I/O counters that *are* gated
(`CAP_SYSINFO_KERNEL`) stay the Switchboard's alone: a pane reporting how full
each volume is does not need them, and the health a capsule states is already
in the mount record as the live availability overlay a degraded or recovering
device sets.

**The walk never runs on the loop that owes a frame.** It is an IPC round
trip, so it goes to a worker on its own wait-set token: the pane asks when it
comes on show, draws whatever has already arrived, and rebuilds when the
answer lands as an ordinary wake. Coming back to the pane asks afresh, because
unlike the shipped picture store the mount table moves.

## Networking

Two composed panes and two stated absences, and the split between them is the
authority line the whole application is built on.

**TCP/IP** is the stack-wide policy: whether the machine speaks IPv4 and IPv6
at all, whether it forms temporary IPv6 source addresses, and the three
behaviours every TCP connection shares — the connection-flood defence, whether
an idle connection is probed, and whether routers may signal congestion
instead of dropping packets. These are the `net.*` keys of the same
`system.conf` store Login & startup and Caching stage, so the pane is a third
composition over the same rows and the same one `configure` run; it adds no
form machinery and no second writer. Applying it takes effect **without a
reboot**: `configure` writes the store and then hands the policy to the
running stack over its `CAP_NET_ADMIN` admin endpoint, and a refusal there
(no stack running, or an account that may not) leaves the saved setting
standing for the next boot and says so rather than claiming it applied.

A row whose effect something above it has taken away says so and keeps its own
value, exactly as a cache class does under the master switch: the temporary-
address row states that IPv6 is off, and the three connection rows state that
a machine with neither family makes no connections at all. The value is still
what the store holds — that is what would apply if the switch above came back
on — but a reader who saw `On` alone would believe it was running.

**Ethernet's live readings stay the Switchboard's; its *configured*
addressing is read, and changed, by an authenticated run.** An interface's
link state, bound addresses and throughput need `CAP_SYSINFO_GLOBAL`, and its
hardware identity needs `CAP_SYSINFO_HW` — the MAC is stable hardware identity
and the address book is system-wide, cross-principal state. Settings holds
neither and never will, so those readings stay the
[Switchboard](switchboard.md)'s, exactly as the per-device I/O counters do for
Storage. `network.conf` cannot be served ungated either: it carries the
`match.mac` identity and the static addressing those two gates exist to
protect, so serving the document would be a way round them rather than an
answer to them.

What the pane *can* do is read the configuration the way an administrator
would — by being one — and then change it the same way. It is a small state
machine. It opens saying nothing has been read and offering **Show
Addressing…**; the reader offers an account, and the supervisor runs
`configure` as it and relays what it printed back through the
elevated-**read** seam (`ElevateRequest::Capture`, see
[login](../userland/login.md)). Every `<interface>.<setting>` line of that
listing is parsed back through the shared `lib/netconfig` engine as the
document it came from — the machine settings in the same listing are dropped,
and a listing the engine will not take whole is no document at all rather than
the part that happened to parse. One plate per interface then, discovered from
that document and labelled in a reader's words. A run that printed more than
the reply carries states that it was too large and shows no part of it; a
refused run states the refusal and leaves the pane saying nothing was read.

With a document in hand the plates are **settable**: the addressing rows
(both method rows, both addresses, both gateways, the MTU and the interface's
own name servers) are controls, while `kind`, the two `match.*` keys and the
`bond.*` keys stay readings — which device an alias stands for and how a bond
is composed are not a settings pane's to change. A method row's choices come
from the key's own `ValueShape`, so the set a reader is offered and the set the
parser admits are one definition; its leading entry is the one thing only the
document can say, that the key is not declared at all. An entry's empty value
is that same removal, which is what lets an interface be moved off a static
address at all.

**The working copy is the reader's edits, not an edited document.** A
document is only ever checked whole, because neither half of "drop the static
address" and "switch the method to DHCP" is a document the parser accepts on
its own. So the pane holds the changed keys, checks each value against its own
key as it is typed — a refused value wears the refusal, keeps exactly what was
typed, and stops Apply rather than being quietly dropped from the change — and
checks the whole document once, before it asks for a password. A document that
would not hold together is refused in the band, naming what is inconsistent,
rather than by a run the reader has just authenticated. Each plate says on its
own caption how many of its rows are staged, so the band's count names a part
of the pane.

Apply is one elevated `configure` run carrying every changed key as a
`<key> <value>` pair, so the document is rendered once and cannot be left
holding half a change. A clean exit is recorded rather than re-read: the tool
applies every named pair or none, and both sides render through the same
engine, so what the pane shows afterwards is what it asked for — for the keys
it named, which are the only ones it claims to know. Leaving the pane drops
the capture, so a reader who wants the document as it now stands asks for it
again, and a privileged reading never sits in this application while the
reader is elsewhere. Moving between Ethernet and DNS keeps it, because both
are discovered from the same document.

A plate whose interface declares neither `match.mac` nor `match.node` says so
beneath its rows: no device can ever be bound to it. `configure` states the
same limit when such an interface is written, onto a console a desktop reader
never sees, so the pane says it first.

**DNS** states the recursive name servers the stack is actually resolving
through: the statically configured and the DHCP-learned servers, aggregated
and deduplicated by the stack into the one answer a userland resolver client
reads too. One row per server, discovered rather than declared, from the
ungated `NET_RESOLVER_SERVERS` query — public host configuration, the TAIRiX
analogue of a world-readable `resolv.conf`. An empty set and a reading that
could not be taken are kept apart: the first says the machine resolves no
names, the second says the reading is not measured. The walk is an IPC round
trip, so it runs on a worker like the mount walk, and returning to the pane
asks afresh because leases come and go. Beneath that live plate the pane
offers each interface's own `dns.servers` over the same capture and the same
one `configure` run the Ethernet pane uses — one definition of what the key
is, composed into two panes, rather than two surfaces that could disagree.

**Wi-Fi** states the absence of the subsystem: no 802.11 driver, no
supplicant, and no vocabulary for a scan or an association.

## Users & Groups

The one pane built from three readings of **different authority**, because no
single query is a Users pane and widening one until it was would be a real
loss.

The caller's **own account** — its name, full name, user id, primary group,
memberships, home and shell — is the ungated `SELF_ACCOUNT` query, resolved by
the service against the uid the kernel attested rather than one the request
names, so there is no parameter for whose account to read and no path to
another principal's. It carries no capability ceiling, no lock state and no
password material: a principal reading its own details crosses no boundary,
and none of the rest is needed to render them. A uid no database holds is
stated as exactly that, apart from a reading that could not be taken.

The **roster** is the ungated `USER_DIRECTORY`, walked paged: every account's
name and user id, the `/etc/passwd`-class public pairing, and nothing else.
The plate says so in its own footnote rather than fabricating the rest —
because an ungated lock state is an enumeration of which accounts are live and
so worth attacking, and an ungated shell and home path are reconnaissance any
unprivileged process, a compromised parser sandbox included, learns nothing of
today.

The **groups** plate is the sibling ungated `GROUP_DIRECTORY`: rendering a gid
is the same display need as rendering a uid, and it is what every membership
row above it reads a group's name through. A gid the directory does not carry
renders as its number, which is the honest answer rather than a fabricated
name.

Everything else — another account's fields, any account's lock state, its
capability ceiling — is `CAP_USER_ADMIN`'s, and Settings holds no capability at
all. So the band's command is a **capture**: an authenticated run of
`users --list`, whose relayed output the pane reads back through the one
`:`-delimited listing form `lib/useradmin` defines for both the tool that
prints it and the surface that reads it. A reply larger than the supervisor
carries answers *overran*, so the pane either holds a listing it can draw or
says plainly that it holds none; output that is no listing at all is refused,
while a single line the grammar does not admit is skipped, so one unreadable
record never hides the rest. The listing is dropped the moment the reader
leaves the pane, and a capture that lands for a pane the window has since left
is dropped rather than installed — the desktop can send the window elsewhere
while a run is in flight, and a privileged reading is never adopted by a
surface that did not ask for it.

Once it lands there is one plate per account, captioned by its name and uid,
whose full name, primary group, other groups, home, shell, login state,
capability ceiling and a new password are all settable. A **service identity**
gets readings rather than controls for its home, shell, login and password,
with a footnote saying why: the database refuses a record shaped otherwise, so
offering those rows would be offering a change that can only ever be refused.
The login row offers exactly the two states `usermod` can set, plus the
account's own where it is neither. Group rows are read by name and applied by
number, and a name the machine does not hold is refused on the row.

Apply is **one** elevated run, and the pane refuses anything that is not:

- a change spanning more than one account, because one command changes one
  account and a change split over two runs can leave half of it durable;
- a password together with the fields beside it, because the password is set
  by its own command.

Both are stated before anyone is asked for a password rather than discovered
after. A **password never leaves this window as a password**: the row is a
masked field whose bounded buffer zeroises what it discards, the record is
built here from `lib/users`' own PBKDF2 builder under a salt the caller drew
from the kernel CSPRNG, and `passwd --record` receives the record. A draw that
produced no salt refuses the apply rather than reaching for a predictable one,
and a salt is spent once. The plaintext is read from its field by borrow at the
moment it is hashed and is copied nowhere — a plaintext in a second buffer is
one no erasure can reach.

Every write elevates a **named** account and the kernel decides. `users_admin`
is gated whole, so a principal editing its own record still needs that grant;
an unprivileged self-service password change would be a new authority path
rather than a wider gate, and this surface offers none. The pane offers the
action, the kernel refuses it where it must, and the pane states the refusal.
The never-widen grant rule and the last-administrator guard remain the only
arbiters; nothing here pre-approves an escalation.

A run that exits cleanly moves the listing on to hold what was applied rather
than dropping it: `usermod` applied every field it was given or refused the
run, so re-reading would cost a second password for an answer already known,
and the reader is left in place for the next change. The public directories
are free, so they are read again at once.

## Notifications, Mouse, Keyboard, Lock Screen and Screensaver

Five more panes over the desktop's own document, each **immediate** and each
posting only its own keys, so no pane can reimpose a value another pane set.

- **Notifications** is a desktop-wide switch (`notify.enabled`) and one row
  per source — the bundle the kernel attests posted a notice, never a name the
  program gave itself — offering *All notifications*, *Warnings and critical*,
  *Critical only* or *None* (`notify.sources`, one `<bundle>:<level>` entry
  per source that does not show everything). A source is listed once it has
  notified since the desktop started, which Settings asks the session through
  the `QueryNotifySources` window request that the session answers to its own
  Settings application alone, or once the policy holds a level for it, so a
  source quietened in an earlier session stays reachable. With neither the
  plate says *None*, which is the truth. The policy's spelling must fit one
  settings value; a change that would outgrow it is refused, and the row goes
  back to saying what is in force.
- **Mouse** sets which button is primary, the pointer speed, and the
  double-click interval. The interval is the one the whole desktop uses: the
  session publishes it in `DesktopInfo`, and the window manager's title bars,
  the desktop's icons and every application pair presses under it.
- **Keyboard** sets how long a key is held before it repeats and how often it
  then repeats, or that it does not. The session repeats the held key itself
  and drops a device's own repeats, so every keyboard behaves alike. It states
  what it cannot offer: this system has one built-in layout and no list of the
  desktop's shortcuts.
- **Screensaver** sets how long the desktop sits idle before the screensaver
  covers it, and what it shows: black, the desktop's own backdrop dimmed, or
  the shipped pictures one after another.
- **Lock Screen** sets how long the desktop sits idle before the screen locks,
  and offers **Lock Now**, which asks the session for its own lock through the
  `LockScreen` window request — answered for this application alone, because a
  lock any program could raise would keep the user out of their own desktop.
  It states that unlocking always asks for this account's password; that is not
  a setting. A refused request is stated on the row that asked.

The document spells every span a person edits in whole units — milliseconds
for the pointer and keyboard, minutes for the idle waits — and every in-memory
and wire form of one is a `Duration64`. A value set off the offered ladder is
offered under its own value, so opening a pane never changes it.

## Absence is stated, never mimed

A control that would change nothing is never drawn. Each pane declares what
backs it, and the three answers are different facts to a reader:

| Backing | What the pane says |
|---|---|
| it composes real controls | nothing — the rows are what it says |
| nothing in this system can serve it | what is missing, and what would have to exist |
| the readings and writes exist, and this surface does not yet compose them | what the pane will show, and where the setting is read or set today |

The stated absences draw through one renderer, quiet and on the surface
behind them with no plate — the same shape every other stated absence in the
desktop takes, because a plate would read as something to interact with.

No pane takes the third answer today: every category this system can serve
composes its own controls, and the rest state what is missing. It stays in the
vocabulary because it is the honest thing for a category whose readings exist
before its controls do.

Six of the categories a desktop should offer have no subsystem beneath them
on this tree at all: there is no Bluetooth stack, no print or scan stack, no
touchpad or touch driver, no 802.11 driver, and no file- or screen-sharing
server. Sound has a subsystem — programs play through the audio service — but
nothing that sets a device's volume or picks the default device, and Settings
must not draw a volume slider that changes nothing. So those categories are
present, reachable, and honest: each states what is missing and what would
have to land.

## On a running machine

The `settings_qemu_aarch64` vertical opens Settings from the capsule's system
menu and photographs it on General, on Lock Screen, on a stated absence, and
on Storage —
reached past the fold of the strip by the strip's own scrollbar — each dump
gated on the desktop session's witness that the frame carrying that pane's
title is on screen. It then pages the strip back up, chooses Light on
Appearance and photographs the
desktop redrawn light, and passes only once the desktop's published settings
document has been committed twice: for that choice, and for the system menu's
*Dark Appearance* row, which reaches the same document through the same
persist-then-adopt path.

## Authority map

One row per pane: what backs its readings, where a change goes, and what a
refusal looks like. `plans/NEW-DESKTOP-SETTINGS.md` §2 is the full table and
the staged source of truth; the shape of it is:

- **read-only panes** (About, Date & Time, Storage) render a reading, or render
  *unmeasured* when the reading could not be taken — never a fabricated zero.
  About states the machine's name, machine id, OS version, uptime, processor
  and memory, each from its own ungated `sysinfo-v1` query, so one refusal
  costs one row rather than the pane; Date & Time states the wall clock and
  which source it came from. Storage draws
  one card per mounted volume from the ungated `MOUNT_LIST` query, derived by
  the shared volume view model every other surface reads, so it needs no
  capability beyond the two the bundle already requests; the per-device I/O
  counters that do need `CAP_SYSINFO_KERNEL` stay the Switchboard's alone.
  The walk is an IPC round trip, so it runs on a worker and the pane draws
  what has arrived rather than waiting;
- **user-scope panes** (Appearance, Wallpaper, Lock Screen, Screensaver,
  Notifications, Keyboard, Mouse, Accessibility) post a document to the
  desktop session, which validates it, applies it and persists it to its own
  published app-data scope; the desktop adopts a change only after the write
  succeeded, so memory and disk cannot diverge;
- **machine-scope panes** (Login & startup, Caching, TCP/IP, Ethernet, DNS,
  Language & Region) ask the console's elevation broker to re-authenticate an
  account that may, and run the same `configure` program the command line
  uses, so the CLI and the GUI are literally the same writer. They are
  **staged**: a choice edits a working copy, the pane's action band says how
  many rows differ, and **Apply** asks for an account once and runs
  `configure` once, carrying every changed key — so the document is rendered
  a single time and a group of settings can never be left half written. A
  refusal leaves the working copy intact and states why; nothing is reported
  applied that was not. The two stores differ only in how they are *read*:
  the boot-time one is read through the ungated `SYSTEM_CONFIG` query and
  parsed with `lib/sysconfig`, while the network one is not public at all and
  is answered by an authenticated run of the same tool (above). Either way
  the row and the writer read one engine, so what a row shows and what the
  tool would set cannot disagree;
- **kernel-scope panes** (Date & Time, Users & Groups) elevate the application
  that owns the syscall, never acquiring the capability here. Date & Time's
  action band starts `datetime.app` as an authenticated account and leaves it
  running — a window that waited for a program the reader then works in would
  stop drawing for the whole session. Users & Groups elevates the account
  tools instead and *waits*: a capture of `users --list` to read what no
  unprivileged caller may, and one `usermod` or `passwd` run to write it. It
  reimplements none of them and holds no path to any of them without a
  password.

The credential question every elevated run is offered through is the shared
`lib/controls` credential sheet, the same surface the desktop session puts up
when a command it may not perform is chosen: one credential surface on the
desktop, with one focus order, one refusal wording, and one place the secret
lives. It is modal while it is up, so a press behind it cannot change a pane
the reader is about to authenticate for, and the password is held only in the
masked field's bounded buffer, which zeroises what it discards.
