# Switchboard monitor service

`userland/gui/switchboard` (`tairix-switchboard`) is the **Switchboard
monitor service** (`plans/NEW-TASKBAR.md` T10–T12): a small, dedicated
process the desktop session spawns as the logged-in user, which samples the
live system through the System Information API, feeds the taskbar's
always-right-most Switchboard icon its tray signals, and hosts the live
overview window that icon opens.

## Role

The tray overview needs system-wide authority the desktop session's own
manifest should never carry. Isolating the sampling in its own
capability-sized process keeps that authority out of the session
(`AGENTS.md` §5.2): the session merely receives compact summaries over IPC
and hands them to the taskbar's tray model.

Each 2-second cycle samples the process list (stopped-process count and the
top CPU consumer since the previous sample, keyed on the stable
`proc_id`), the aggregate CPU busy fraction, and — every fifth cycle, to
bound the audited query's rate — the kernel memory-pressure band. A pure
derivation turns the sample into the wire `TraySummary`: CPU pressure with
enter/exit hysteresis (≥ 900‰ / < 800‰), the dominant of the CPU/memory
pressures with a pressured-resource count, and a validated top-task name.
Every field is a real measurement or an honest absence — a failed or
refused query degrades exactly the field it backs, never fabricates one.

## Channel

Summaries travel over the seat-scoped `SWITCHBOARD_ENDPOINT`
(`lib/abi/src/switchboard_ipc.rs`), which the **session** binds and this
service calls as a client. Publication is change-only against the last
acknowledged summary, with a 10-second keepalive that doubles as orphan
detection. A successful publish replies with the serving session's
`ProcId`, which is how this service learns the one identity it will accept
commands from.

Commands travel the other way, over the per-instance mailbox
`command_endpoint_for(<own pid>)` that this service binds: `OpenPanel` (show
the overview on a named section), `SeatReport` (which window owners the
session's liveness vigil finds unresponsive), `Power` (the machine
transition the user confirmed in the taskbar's quick-actions menu — see
[Power transitions](#power-transitions)), and `FrameReport` (what the
session's last composited frame cost — see
[The Desktop block](#the-desktop-block)). Every command is authenticated
against the kernel-attested sender of that very message, never a claim on
the wire; a command from anyone but the attested session, a command that
arrives before any session has been attested, and a frame that does not
decode are each dropped with a stated reason and never touch the model.

## The live overview window

`OpenPanel` shows this application's own `Switchboard` screen composition
(`src/view/`, one module per section around a shared skeleton) on the
requested section, through `Switchboard::select_section`. The screen is
assembled entirely from the shared `lib/controls` controls and paints no
chrome of its own; it lives in the application rather than in `lib/controls`
because it arranges those controls into one particular window
(`plans/NEW-SWITCHBOARD.md` S1).

The window manager decorates the window server-side (the frame, title bar,
window commands, and resize grabber — see `plans/COMPOSITOR-WORK.md`);
Switchboard draws only its client content, beginning with a **location band**
at the top: a `Breadcrumb` reading `Switchboard › <section>` and, at
its trailing end, an `IconButton` that opens a `Menu` of the six sections with
the one on show marked selected. The trail's leading crumb opens the same
list, so the section is reachable by pointer or keyboard — with the band
focused, Space or Enter opens the list, Up/Down walk it, Enter shows the
section under the cursor, and Escape closes it unchanged. There is no tab
strip: the band is the whole section switcher, and both routes run the one
transition `Switchboard::select_section` runs.

There is at most **one** window: a second
`OpenPanel` asks the session to raise the existing one (naming this
service's own pid) and switches section rather than stacking a second. The
window's close control destroys it and the service returns to headless
sampling — sampling and publishing continue unchanged whether or not a
window is open, because the window is a view onto a monitor that never stops
monitoring. The system is re-sampled strictly on its 2 s deadline: an input
or command wake never re-queries the system.

The model is rebuilt on the same sample cadence, and the panel presents at
most once per wake, and only when what it would draw differs from what it
last drew — the composition itself, the window's bounds, the active
theme, and the render scale. A wake that delivered an event but left every
one of those unchanged, such as a pointer move that crosses no control,
costs no render and no present. What it carries:

| Section | Source |
|---|---|
| Tasks | the sampled process list, as a filterable, searchable, sortable table with the selected task's commands beside it — see below |
| Pressure | "why is my machine slow": one cause card per resource the **tray's own latches** flag (CPU ≥ 90 % with < 80 % release; memory band ≥ mild), naming the measured culprit — the busiest sampled task for CPU, the largest mapped address space for memory — with a plain-language cause line and recommended actions, each rendered `Ready`, `DisabledByState` (a culprit already at `Low` priority or already stopped) or `DeniedByAuthority` per the same rule the kernel enforces. No latch, no card; no per-task rate yet, a culprit-less card with the one action that is still honest (`Show tasks`) |
| Activities | the service's own **session-lifetime task groupings**: named sets of live processes (keyed by the never-reused `proc_id`), each rendered with its member rows joined against the current sample. Created from the Tasks section's `Group…` command; renamed inline; paused/resumed/closed as a set |
| Recovery | stopped processes this service sampled itself, plus the seat report's unresponsive owner ids **joined against those same sampled names** — the report carries ids only, so an owner this service never saw produces no row rather than a fabricated one |
| System | the machine's own readings, over eight sidebar pages beneath four header tiles — see below |
| Jobs | always empty — the list states the absence rather than reading as "nothing is running"; see below |

A resource the service could not measure this cycle reads `unknown` with an
unmeasured meter, never a fabricated `0%`.

### One anatomy, one drop order

Every section lays out into the same frame — an optional sidebar, header and
footer around a primary column with an optional detail pane, impact column and
action rail beside it — resolved in one place, so no section improvises its own
geometry (`plans/NEW-SWITCHBOARD.md` S3).

A window too narrow to seat everything **sheds** the optional columns in a
fixed order — detail, then impact, then rail, then sidebar — rather than
squeezing the primary column. What the primary column may not fall below is a
floor each section declares: a section whose rows carry inline commands states
how many, and the frame turns that into the width that strip actually needs, so
a row's commands can never be pushed off its own edge. That arithmetic has one
definition, shared with the row splitter that lays the buttons out, and the
window's own minimum client width is the widest such floor — not the width at
which every optional column happens to fit, because shedding one is a correct
outcome and clipping a command is not.

A section whose primary column is a list of `Card`s — Pressure, Recovery, and
Background — is a master/detail screen, and **pressing a card selects it**: a
completed click anywhere on a card's own body, clear of any footer button it
carries, makes that card's subject the selected one, so its detail pane, impact
column, and action rail all describe the card the reader just pressed. Where a
card carries footer commands (only Pressure does), a click on one of those
buttons selects the card *and* resolves that command, so a command can never
act on a subject other than the card that offered it. A card that is not
actionable — disabled, or denied by authority — selects nothing. One walk over
the visible cards serves all three sections, so no section can drift into a
different idea of what a press means, and the keyboard cursor selects the card
it lands on for the same reason.

### The Tasks table

Tasks is a census in the location band, a header band, the rows, the selected
task's commands beside them, and a footer band (`plans/NEW-SWITCHBOARD.md` S4).

The window's **location band** carries the table's census: four plated
`MetricTile`s — Processes, Jobs, Services, Alerts — beside the trail naming
where the reader is, each with the glyph of the thing it counts. The band grows
to seat them and shrinks back for a section that has no census; a window too
narrow to seat both drops the census rather than abbreviating the reader's own
location.

The **header** carries a filter `Tabs` strip (All, Processes, Jobs, Services,
Faults) whose labels carry each filter's own count, and beneath it a
full-width `SearchField` that matches on the task's name, case-insensitively —
*which kind* of task and *which* task are separate questions, so each gets its
own row. Every tile and every tab counts the adopted rows through the *same*
predicate the filter itself applies, so a tile, its tab and the rows it shows
can never state different numbers. Filtering, searching, grouping and sorting
are arrangements of the rows already sampled; none of them issues a new query.

A sample changing a count re-labels those tabs **in place**. The strip is built
once and holds one tab per filter for the life of the section, because it — not
the section — is what remembers which tab the pointer rests on, which one the
keyboard cursor is on, and which one a press is waiting to complete on. A
fresh strip each sample would know none of the three, so a count moving under a
resting pointer would blink the highlight off and swallow a click in flight.

The **rows** are a sortable `TableHeader` over nine columns: Task (its icon and
name), Type, State, Activity, CPU, Memory, Disk, Network, Last active. Every
column is a *reading* about the task. The sort is the header's own, applied
over the filtered rows and stable — rows a column cannot separate keep the
order the sample reported them in. *Activity* is the task's own CPU sparkline,
drawn into that column's rect; the column geometry has one definition, which
the heading, the cells and the sparkline all read. A working task draws no line
under its row: the trend belongs in the column whose heading promises it.

The **commands** are an `ActionRail` captioned `ACTIONS`, anchored to the right
of the table so they stay still while the rows scroll: Switch to, Reveal
window, Pause, Resume, Lower priority, Open logs, Group…, and Force quit, each
with its own glyph. They act on the **selected** task — clicking a row selects
it, and a table with rows always has one selected — which is what lets the list
name a task's whole repertoire rather than the one or two buttons a row could
hold. Force quit carries the destructive weight and sits last. Each command
renders its own verdict: permitted, plainly disabled where the task's state
rules it out (resuming a task that is not stopped), or the Authority Mark where
the caller lacks `CAP_PROC_CONTROL`. *Open logs* is always disabled: no
capability-gated query for a task's own log entries exists yet, so the command
states its absence rather than pretending to work.

The **footer** states how many rows are shown of the total and carries an
Auto-refresh `Toggle` beneath the table — holding it on the sample the reader
is reading rather than letting it move under them — and the grouping `ComboBox`
(ungrouped, by type, by activity) beneath the commands, so each control sits
under what it governs.

The content cursor spans the header controls, then the rows, then the commands,
then the footer controls, so every control is reachable from the keyboard
whatever the filter leaves showing — including nothing.

Type names what a row *is*, not what it is for: a row from the process list is
a `Process`. `Job` and `Service` are the kinds a job registry and a service
manager will contribute, so their tiles and tabs read a genuine zero today
rather than a guess. Three filters the concept boards sketch are deliberately
absent, because no reading backs them: *Background* (the process list carries
no foreground/background signal), *Recent* (there is no last-active interface),
and *Hung*, which is folded into *Faults* — one filter over the same classifier
the Recovery section uses, so the two can never disagree about which tasks are
faulted.

#### What the table measures, and what it cannot

*CPU*, *Memory*, *State* and *Activity* are measured per sample. *Disk* is a
real rate: the service deltas each task's read-plus-written byte counters
against that task's *own* previous reading over the interval between the two
samples. A cumulative total is not a rate, so the first sample, a task seen for
the first time, and an interval nobody measured each yield no reading; a
counter that did not move over a real interval is a genuine `0`. *Activity*
plots a bounded per-task ring of the CPU shares already measured, keyed by the
never-reused `proc_id` so a recycled pid cannot inherit a dead task's history
and an exited task leaks neither its history nor its counters.

*Network* and *Last active* have no interface at all — there is no per-process
socket accounting, and the process record carries no creation timestamp — so
both render the explicit unmeasured mark. An absent reading is never a `0`,
never a dash that reads like one, and never a plausible number.

### The System screen

System is the machine reading about itself. A vertical `Tabs` **sidebar**
selects one of eight pages — Overview, Resources, Storage, Network, Session,
Permissions, Services, Power. It selects the page, not the section; the
window's location band remains the one section switcher.

The **header** carries four `MetricTile`s. CPU and Network plot a trend,
because a rate has no fixed ceiling to fill a bar against; Memory and Disk
fill a track, because each is a fraction of a measured whole. The Disk tile
sums the capacity of every mounted volume and the Network tile sums every
interface's rates, so each states the machine's figure rather than one
arbitrary member's.

Each **page** states what it can measure:

| Page | What it reads |
|---|---|
| Overview | hostname, OS version, machine id, uptime, boot time, CPU model and core count, load average and installed RAM; then the Active Services statement and the authority summary |
| Resources | the per-core load, the memory and kernel-heap detail, and what the desktop's last composited frame cost |
| Storage | every mount — source, mount point, filesystem, medium, availability, used-of-total from the volume's own block counts, and its measured health |
| Network | per interface — its facts, its link state and addresses, and its rx/tx rates |
| Session | the seats and the logged-in census the load reading carries |
| Permissions | what the service can attest about the caller's authority, with each resource limit's soft and hard bound and its live usage |
| Services, Power | no interface exists, so each states plainly what is missing and why |

Every page compiles down to one ordered vocabulary of lines — a heading, a
fact, or an absence — so the screen has a single layout, a single scroll
range and a single paint loop rather than eight of each.

The **rail** is `SYSTEM ACTIONS`, seated in a `Panel` because the rail control
carries no caption of its own. It is a rail rather than a per-row column
because this screen commands one subject: the machine.

**Nothing here is ever fabricated, and an absence says which kind it is.** A
reading the caller may not have reads *not permitted*; one the service asked
for and did not get reads *unavailable*. They are different statements to a
reader and are never conflated. A missing measurement is never a `0`, never an
empty bar, and never an empty list — an empty list reads as "none", which is a
claim, so a reading with no interface behind it states its absence in words.
An unmeasured trend plots no trace at all rather than a line along the floor,
and an unmeasured track stays unfilled rather than sitting at nought.

A refused action names its own kind of refusal. An action refused for want of
a capability wears the Authority Mark, because acquiring that authority would
make it available; an action with no endpoint behind it at all is plainly
disabled, because no grant would change anything.

The content cursor's stops are the eight pages and then the rail's buttons, so
Up/Down walks the sidebar exactly as a reader expects of a vertical list and
Enter or Space commits. The `Tabs` control's own vertical navigation is
deliberately not given the same keys, which would give them two meanings.

#### The Desktop block

The Resources page's third block is the one reading this service does not
sample. The session owns the compositor, so it sends what its last frame cost
over the command channel above — only when the counts changed, never more than
four times a second, and never when the only served content was this service's
own paint (a monitor must not measure itself) — and this service validates and
renders it:

| Row | What it reads |
|---|---|
| Last frame | the pixels the frame recomposed, of the whole screen |
| Blended | the layer contributions blended to resolve them, and how many times over the damage that is |
| Opaque copies | damaged pixels resolved by copying an opaque run instead |
| Rectangles | how many rectangles the frame recomposed |
| Present calls | how many calls into the display driver published it |
| Window furniture | furniture lookups served from the retained cache, and how many had to be rendered |

Each counter's own meaning is in [what one frame
cost](./wm.md#what-one-frame-cost-framestats). *Blended* exceeding *Last
frame* many times over is not an error — it is the reading: a frame that
blends four million pixels to change three thousand is paying for depth
nobody can see, and one that recomposes the whole screen to move a cursor is
damaging too much.

The block is refreshed as each report lands, but only while the window is
open: a report that arrives with the panel closed is adopted and nothing is
rebuilt for it, because rebuilding walks every sampled process to allocate a
row, a name and a history for each, and no window means no reader. Opening the
panel rebuilds first, so the first frame a user sees already carries every
report that arrived while they were not looking — and the session's own rate
limit ([the command
channel](./session.md#the-command-mailbox-the-session-sends-on)) is what
stops a pointer crossing the wallpaper producing those reports at frame rate
in the first place.

A report whose counts no compositor pass could have produced is refused where
it is decoded, so the block never renders a sender's arithmetic. An idle frame
reads *idle, nothing recomposed* rather than a row of zeros pretending a frame
was drawn, and a frame nobody has reported yet reads unavailable. Every figure
is a count of work; no duration rides this path, because a duration is neither
reproducible nor assertable.

### The Recovery screen

Recovery is the one screen about a *single* fault at a time. The **primary**
column is one `Card` per fault — what faulted, what happened to it, and how
long ago — and the three columns beside it all describe whichever card is
selected.

Selection is remembered by the faulting task's own kernel-attested identity,
never by its row number. The list is rebuilt from scratch every sample, so a
number would silently re-point at a different fault the moment one above it
cleared; the identity survives a reorder and drops only when the fault
genuinely goes. That rule has one definition, which every section with a
selection to keep reads.

The **detail** pane names the fault and the task it is, a `StatusPill` naming
what the fault costs while it stands, a `FactList` of its status, its age and
the recommendation, and then a `Tabs` strip over three pages:

| Page | What it reads |
|---|---|
| Timeline | the marks this service observed: the fault itself, stamped with the age it has stood, and — where that age is known — that it is still standing |
| Crash Snapshot | the kernel's own crash record for that task: the fault class and the distance from its anchor, the access direction, the owning uid/gid, `pc`/`sp`/`fp`, every named register and every backtrace frame |
| Logs | no log-query interface exists, so the page states that |

The crash record is matched to its fault by process identity and nothing
else: a numeric pid is reused, so matching on one could attribute a dead
task's crash to a live task that inherited its number. A fault with no record
says so plainly — a task the kernel stopped, or one merely gone
unresponsive, has faulted without ever raising a user fault, so that is a
statement of fact and deliberately does not wear the unmeasured mark.

The **impact** column stacks four unplated `MetricTile`s for the faulting
task's own CPU, memory, disk and network. Network is always unmeasured: no
query reports a process's network use, so the tile says so rather than
showing a zero.

A fault's **age** is tracked by the service, not read from the kernel: there
is no state-change timestamp anywhere in the System Information API, so the
service keeps when it first saw each task faulted, keyed by that same stable
identity and clocked off the monotonic uptime reading. With no uptime reading
there is no clock, and the age reads unmeasured rather than as a fabricated
zero. An entry is dropped the first sample its task is no longer faulted, so
a task that recovers and faults again is timed from its *new* fault.

The **rail** is `RECOVERY ACTIONS` for the selected fault, carrying only the
commands this service actually backs: Restart, and Force with its
confirmation posture (or the Authority Mark when the caller may not take it).
The **footer** states how many faults have cleared. That count is observed
history — only something that folds one sample into the next can see a fault
disappear — so it is counted where the samples meet and carried in the model,
which is what keeps a refreshed screen and a freshly built one the same
screen.

The content cursor walks the fault cards, then the page strip, then the
rail's commands. Moving onto a card selects it, so the detail, impact and
rail always describe the card the reader is on. A rail stop hands the key to
the button, so a refused command refuses the keyboard exactly as it refuses
the pointer.

### The Pressure screen

Pressure carries the same master/detail anatomy as Recovery: the **primary**
column is one `Card` per flagged cause — the culprit, a plain-language cause
line, and that cause's relief commands in the card's own footer — and the
**detail** pane describes whichever cause is selected. There is no action rail:
a cause's relief belongs to the card that offers it, where the resource being
relieved is unambiguous.

Selection is remembered by the *resource* the cause is about, not by its row
number: the list is rebuilt every sample, so a number would re-point at a
different resource the moment one above it eased. A cause that is still flagged
keeps the selection however far it has moved; a resource that eases loses it.
That is the one shared selection rule every section with a selection reads.

The detail pane names the culprit and the resource, then states four facts:

| Fact | What it reads |
|---|---|
| Resource | the pressured resource the cause names |
| Pressure | how much of it is in use — the machine-wide CPU busy share, or the memory band's own measured share |
| In band | how long the resource has stood in that band, measured by this service (below) |
| Relief | the cause's own recommended command, and — where this session cannot take it — why not |

A **band's age** is tracked by the service exactly as a fault's age is: nothing
in the System Information API timestamps a band change, so the service keeps
when it first saw each resource pressured and clocks it off the monotonic
uptime reading. With no uptime reading there is no clock and the age reads
unmeasured rather than as a fabricated zero; a band that eases forgets its
start, so a resource that comes back under pressure is timed from its *new*
band.

The Relief fact names a refused command rather than hiding it: a reader is told
what would relieve the pressure and that this session may not do it (`not
permitted` for want of the capability, `not available in this state` when the
culprit is already stopped or already at `Low`). The command itself still fails
closed at its own button, to the keyboard exactly as to the pointer. A cause
whose model recommends nothing says so instead of volunteering another of its
commands.

### The Activities screen

Activities is a flattened list: one header row per group, its member rows
indented beneath it, and the group's four commands — Switch, Pause-or-Resume,
Rename, Close — inline on the header row itself, where the group that owns them
is unambiguous. The **detail** pane describes the selected group.

Selection is remembered by the group's own stable id, and pressing any part of
a header — its name or any of its four commands — selects that group, so the
pane describes the group the reader just acted on. A refresh that reorders the
groups keeps the reader on the same one; a group that closes loses the
selection. An in-flight rename is re-located the same way, by id and never by
row.

The pane states the group's own name and member count, then its four **combined
readings**, then one line per member:

| Reading | Where it comes from |
|---|---|
| CPU | the sum of its joined members' own measured shares |
| Memory | the sum of its joined members' own resident sizes |
| Disk | the sum of its joined members' own measured I/O rates |
| Network | always unmeasured: there is no per-task network accounting to total |

There is no per-group accounting anywhere to read, so a total is a sum of the
members' *own* measurements or it is absent. One unread part makes the whole
total absent — a total that quietly skipped an unmeasured member would
understate the group while reading as a measurement — and a group with no
member joined to this sample has no total at all rather than a nought that
would claim the group costs nothing. Each member line reads that member's own
figure, states plainly that it is not running when it has exited, and wears the
unmeasured mark when it is running but its figure was not read.

### The Background screen

Background carries the same master/detail anatomy: a `Card` per job, a detail
pane (the job's name and how far through it is, a throughput `Chart`, two
columns of `FactList`, and a `Timeline`), a `JOB ACTIONS` rail with Pause and
Cancel for the selected job, and a footer holding an Auto-throttle `Toggle`.

Nothing in this system keeps a registry of background jobs — no service
publishes one and the System Information API has no query for one — so the
list is empty on every real machine and says so in two statements: the shared
absence line ("no interface"), and a sentence naming what is missing, because
a reader who sees only an empty list concludes nothing is running, which is a
different and unverified claim. The anatomy around it is the shape a registry
would fill, not a promise that one exists, and no job is invented to populate
it.

The Auto-throttle switch is off and *plainly disabled* rather than wearing the
Authority Mark: the caller's authority is not what is missing, so the control
must not imply that a grant would change anything.

### Activities are live groupings, not saved workspaces

An activity is a **grouping of live processes for the current session**,
held by this service in memory: members are ephemeral processes, so a
persisted grouping would outlive the only things it names. Members that
exit are pruned (and an emptied group dissolved) — but **only on a sample
whose process list actually succeeded**, so a degraded sample can never
wipe the user's groupings. Set actions (pause/resume/close/switch) act
only on members **joined to the current sample** by `proc_id`: a stored
numeric pid whose process has exited may have been reused by an unrelated
process, so acting on unjoined members is refused by construction rather
than risked. Names are bounded (48 characters), trimmed, unique per
instance, and validated on rename — a refused rename is stated on `stderr`
and changes nothing. The bounds (12 activities × 32 members) are
hand-curation scale; the panel renders the honest reason ("Activity limit
reached", "Activity is full") on the controls they disable.

### What is deliberately empty, and why

- **Jobs** — there is no background-job registry anywhere in the OS to
  enumerate, so the Background screen's list states that absence while its
  anatomy stands ready for one.
- **Services** — the System Information API (`lib/abi/src/sysinfo.rs`) has
  no service-enumeration query, so the System screen's Services page and its
  Overview's Active Services block both state that absence rather than
  showing an empty list that would read as "no services are running".
- **In-panel system actions** — the machine's power transitions are not
  rows *in this window*: they live in the taskbar's quick-actions menu,
  where the user confirms them, and reach this service as the `Power`
  command below. Session lock is the desktop session's own surface (it
  keeps the session running behind it), never this service's.
- **Disk and network pressure cards and resource rows** — the API exposes
  no whole-machine disk-throughput query (the per-process byte counters the
  Tasks table's Disk column derives from measure one task, not the device),
  and while a per-interface network-rates query exists
  (`NET_INTERFACE_RATES`), no tray latch is derived from it, so a card would
  be a guess rather than a measured cause.
- **App "sleep" and disk "throttle"** (sketched on the concept boards) —
  no such kernel interfaces exist; the pressure cards offer only actions
  that genuinely work today: pause (`Stop`), lower priority, show tasks.

Offering a control that would fail at the point of use is worse than an
honest absence, so these stay empty.

### Actions

| Control | Effect |
|---|---|
| Task *Switch to* / *Reveal window* | `SwitchboardRequest::ActivateOwner { owner }` to the session — raising the window is how this system shows a reader where it is, so both commands make the same request |
| Task *Pause* / *Resume* | `signal(pid, Stop)` / `signal(pid, Continue)` on the selected task — requires `CAP_PROC_CONTROL` |
| Task *Lower priority* | `sched_set_priority(pid, Low)` on the selected task |
| Task *Force quit* | `signal(pid, Kill)` on the selected task — requires `CAP_PROC_CONTROL` |
| Task *Open logs* | nothing: no journal-read query exists, which is why the command is disabled |
| Task *Group…* menu | file the task into an activity / a new activity / out of its activity (service-local state; no syscall) |
| Pressure *Pause* | `signal(pid, Stop)` on the measured culprit |
| Pressure *Lower priority* | `sched_set_priority(pid, Low)` on the culprit — lowering follows the kernel's own-child / same-principal / `CAP_PROC_CONTROL` target rule, and the card renders the action spent once the record already reads `Low` |
| Pressure *Show tasks* | resolved inside the widget: jumps to the Tasks section focused on the culprit row |
| Activity *Switch* | one `ActivateOwner` per joined member, raised in reverse order so the first member lands frontmost |
| Activity *Pause* / *Resume* | `signal(pid, Stop)` / `signal(pid, Continue)` swept over the joined members — one refusal is reported and the sweep continues |
| Activity *Close* | `signal(pid, Terminate)` swept over the joined members (the graceful ask — force-kill stays Recovery's job), then the grouping dissolves |
| Recovery *Restart* | `SwitchboardRequest::RestartOwner { owner }` to the session |
| Recovery *Force* | `signal(pid, Kill)` — requires `CAP_PROC_CONTROL` |
| Window *Close* | destroy the window, return to headless sampling |
| `Power` command (from the session) | `system_power(action)` — requires `CAP_SYSTEM_POWER`; see below |

Every row's availability reflects what this service can *genuinely* do: it
queries its own effective capability set through `cap_query`, compares each
row's kernel-attested owner uid against its own, and a control whose
authority is absent renders with the Authority Mark and is never attempted
— the same verdict is re-derived at apply time from the same inputs, so
render and enforcement cannot disagree. A sampled task id that does not fit
the `signal`/`sched_set_priority` signed width is refused rather than
truncated into a different, arbitrary process. A refusal from the kernel or
the session is stated on `stderr` and leaves the model untouched; it never
ends the service.

### Power transitions

Restart and Shut Down are drawn by the taskbar, confirmed by the user in the
session's modal dialog, and **performed here**. The desktop session
deliberately holds no power authority: it is the largest, most exposed
process on the seat, so the widest-blast-radius capability in the system
stays out of it. It relays the confirmed choice as one `Power` command on
this service's authenticated mailbox, and this service — already seat-scoped,
already authenticating that mailbox, already stating its refusals — performs
the capability-gated `system_power` syscall under its own identity.

The check happens twice on purpose. This service refuses without
`CAP_SYSTEM_POWER` before asking the kernel anything, and the kernel checks
the caller again on the far side of the trap. A granted transition never
returns; a refusal (an absent capability, a platform with no primitive for
the transition) is stated on `stderr` naming the transition that did not
happen, and the machine keeps running.

Whether the capability is genuinely held is **published, never assumed**:
every tray summary carries a `power_capable` flag re-read from this
service's own effective capability set at that moment, so an authority the
user's ceiling withholds — or one dropped since start-up — stops being
advertised on the very next publish. The session passes the flag through to
the taskbar, which renders the two rows with the Authority Mark and emits
nothing while it is false. An absent, dead, or not-yet-published service
leaves them denied: fail closed, never optimistic.

## Waiting

The loop is tickless and event-driven: **one** `waitset_wait` per iteration
covers the termination signal, the command mailbox, the machine's
memory-pressure band, and — only while a
window is open — that window's event mailbox, with a timeout equal to the
time until the next real sample is due. Sampling is strict: a cycle
triggered by an input or command wake before the deadline is a no-op that
never re-queries the system. There is no poll loop and no
sleep. The window's event source joins the set when the window opens and
leaves it when the window closes, so a closed window's channel is never left
armed. The periodic re-sample is the documented polling fallback: the
system-wide metrics expose no change event to park on.

## Capability sizing

The manifest requests exactly `CAP_CONSOLE_WRITE`, `CAP_SYSINFO_GLOBAL`,
`CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_HW` (the hardware inventory — the
per-interface network facts the Network page and the network tile are built
from, and the seat list the Session page reads), `CAP_SHM` (the zero-copy
window frame region the session maps, as for any windowed app),
`CAP_PROC_CONTROL` (delivering a control signal to a task this service did
not spawn — the Force action), and `CAP_SYSTEM_POWER` (the machine
transition the session relays here rather than performing itself); the
kernel intersects them with the launching user's ceiling at spawn, so an
ordinary account's instance simply publishes that it is not power-capable
and the desktop's power rows stay refused. The three optional sampling
scopes are probed **once** at startup (capability sets are fixed at spawn;
re-probing would only spam the audit log with denied audited queries):

- an administrator's instance sees the system-wide process list, the
  memory-pressure gauge, and the hardware inventory;
- an ordinary user's instance degrades cleanly to self-scope — its own
  processes, the ungated overall CPU fraction, no memory signal, and no
  interface or seat inventory.

A refused scope is an answer, not an error; the service publishes what it
can honestly see.

## Lifecycle

Started by the desktop session after login; never by PID 1. Exits — each
with its reason stated on `stderr` first:

- a termination signal → clean exit;
- `NotFound` / `PermissionDenied` from the endpoint (no session, or the
  session refused a stale instance) → clean exit — the service has no
  purpose without a session to report to;
- five consecutive **faulty** publish attempts, or a wait-set failure → a
  stated abnormal exit rather than an unbounded silent retry or a busy loop.

`WouldBlock` is explicitly **not** one of those faults. A call endpoint at
capacity refuses the post outright rather than blocking, so a full queue
says only that the session has not drained it yet — the transient
back-pressure condition the kernel defines it to be. It is not evidence of
a fault here nor of an absent session, so it costs the service nothing: the
summary stays unacknowledged, the change gate re-offers it on the next
sample, and one attempt per sample period is paced by the sampler rather
than a retry loop. Counting it as a failure meant a desktop that was merely
busy for five sample periods killed the monitor watching it, and nothing
restarts one — the tray capsule then stayed dead until the user pressed it
again after the session had reaped the corpse.

Design details and constants live in the crate's rustdoc and
`userland/gui/switchboard/README.md`.
