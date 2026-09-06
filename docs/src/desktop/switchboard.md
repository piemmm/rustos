# Switchboard monitor service

`userland/gui/switchboard` (`tairix-switchboard`) is the **Switchboard
monitor service** (`plans/NEW-TASKBAR.md` T10–T12): a small, dedicated
process the desktop session spawns as the logged-in user, which samples the
live system through the System Information API, feeds the taskbar's
always-right-most account capsule its tray signals, and hosts the live
overview window that capsule opens.

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
its trailing end, an `IconButton` that opens a `Menu` of the three sections with
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
costs no render and no present.

A present that does happen covers what the wake's control rounds reported and
no more. The window holds one surface for its whole life, so the render is
clipped to that rectangle and only those pixels are copied into the shared
frame: every pixel outside it is the one already on screen. Every control the
input path reaches reports the rectangle it redraws into one sink the panel
owns, so hovering a row costs the row it left and the row it entered; a
composition-wide transition reports what it re-lays instead (a scroll marks the
content column, a section change the whole client, and opening or dismissing
the section list the pixels the popup covers). A change no control round could
describe — a fresh reading, a resize onto a new surface, a desktop appearance
or density change, or a session that discarded the window's retained pixels —
marks the whole window, and so does a round that moved something and reported
nothing, so an under-report can only ever cost pixels rather than leave a stale
frame. What it carries:

**Three sections, one per question a reader arrives with:** what is running,
what is this machine doing, what broke.

| Section | Source |
|---|---|
| Tasks | the sampled process list, as a filterable, searchable, sortable table with the selected task's commands beside it — see below |
| Resources | one pane per resource *device* the sample names: the processor, the machine's memory, each mounted volume, each managed interface, the display path, and the machine's own identity, seats and authority — see below |
| Recovery | stopped processes this service sampled itself, plus the seat report's unresponsive owner ids **joined against those same sampled names** — the report carries ids only, so an owner this service never saw produces no row rather than a fabricated one |

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

A section whose primary column is a list of `Card`s — Recovery — is a
master/detail screen, and **pressing a card selects it**: a completed click
anywhere on a card's own body, clear of any footer button it carries, makes
that card's subject the selected one, so its detail pane, impact column and
action rail all describe the card the reader just pressed. Where a card
carries footer commands, a click on one selects the card *and* resolves that
command, so a command can never act on a subject other than the card that
offered it. A card that is not actionable — disabled, or denied by authority —
selects nothing. The walk over the visible cards is shared rather than written
per section, so a second card-based section cannot drift into a different idea
of what a press means, and the keyboard cursor selects the card it lands on for
the same reason.

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

### The Resources section

Resources is **one pane per resource device**, instrument-led. A vertical
`Tabs` **sidebar** — the device rail — lists what discovery actually found,
grouped: `Resources` (the processor, the machine's memory), `Storage` (one
entry per mounted volume), `Network` (one per managed interface), `Graphics`
(the display path), then `Machine` (identity and uptime, sessions and seats,
permissions and limits). Each entry carries its name, its current reading and
its own bounded trace, so the rail is a live summary of the whole machine and
the pane is the detail of one part of it. The `Machine` entries carry no
trace: they are facts rather than rates, and the absent instrument is what
says so.

**The rail's length is discovered, never declared.** Twelve cores, four
volumes and three interfaces is the design case; a hundred-core machine with a
dozen volumes gets a scrolling rail, not a truncated one, and no entry count
is a compile-time constant. A machine with nothing mounted has no `Storage`
group at all rather than an empty slot — but a rail group missing because the
*inventory* was refused is a different statement, and the report carries which.

Cores are deliberately **not** rail entries: the CPU pane shows every core at
once, so a per-core rail would state the same readings twice and push the
devices off screen.

Each pane is a **hero** — the device's headline reading, its context lines and
its instrument — over **blocks** of the detail behind it. The instrument
belongs to the reading, not the renderer: a rate trends, because its shape
over time *is* the reading and it has no fixed ceiling to fill a bar against;
a fraction of a measured whole tracks. A fact pane has neither, and that is
what says its readings are facts.

A block holds whatever its reading *is* — a composition, a grid of per-core
cells each with its own trace, the tasks costing the device most, a status
pill the health buckets resolve to, or genuine facts. Rendering a resource as
key/value text is the defect this section exists to fix.

**A resource under pressure wears a banner on its own pane**, above the hero:
the band, how long it has stood there, and the relief the model recommends. A
cause and its resource were never two places. A band's age has no interface
behind it — nothing timestamps a band change — so the service clocks it off
the monotonic uptime reading and reads unmeasured where there is none, never a
fabricated zero.

**A volume's service readings are two-sample deltas, never a served
average.** `VOLUME_IO_STATS` publishes the device's cumulative bytes,
completed requests, busy time and summed waits, and `VOLUME_IO_QUEUE` its
occupancy and the `BlkDeviceClass` budget bounding it; the pane derives
throughput, IOPS, utilisation, await, service time and mean queue depth over
its *own* sample interval, so no consumer inherits another's averaging window.
A first sample, an unmeasurable interval, and an interval in which nothing
completed each state their absence rather than reading as an idle disk. The
two queries carry different gates on purpose — a utilisation figure is one
every user may see, a queue depth is a driver internal — so a session without
`CAP_SYSINFO_KERNEL` still reads its throughput and await while the two queue
rows say which refusal they met.

**The CPU, Memory and volume panes each carry the five tasks costing that
resource most**, from the per-task readings the process record already
provides, so a pane and the Tasks table can never disagree. **Summing them is
not the device's total** and the block says so: filesystem, RAID and swap
traffic belongs to no process. The interface pane has no such block — per-task
network has no interface at all — and states that in words rather than showing
an empty list, because an empty list reads as *none*.

**The Graphics pane is named for the display path, not for a GPU.** A
framebuffer-only or headless machine has no GPU and would read an empty *GPU*
pane — but it still composites, and that work is what a reader needs. So the
pane leads with the compositor's measured frame cost and treats the device as
one of its facts. The reading that earns the block is damaged pixels against
blended pixels against screen pixels; every figure is a count of work, and no
duration rides this path, because a duration is neither reproducible nor
assertable. A frame that recomposed nothing reads *idle* rather than a row of
zeros pretending a frame was drawn, and a frame nobody has reported yet reads
unavailable — only the session that owns the compositor can count one.

**The rail entry states the damage, and its trace plots the damage per
frame.** The rail's figure is what changed on screen; the hero's is the layer
contributions blended to resolve it, two magnitudes apart, so the two are
different readings rather than one stated twice. The trace's full scale is the
frame's own screen: the only reference a per-frame pixel count has, and the
one the hero's context line already spells the reading against, so a
full-screen repaint fills the box and a cursor-sized frame sits near the
bottom because that is what it cost. A byte reference would be the wrong
dimension, and the sample that carries no report contributes no point rather
than a nought that would read as an idle frame.

**The device's own readings come from the display service that drives it**
(`GPU_DEVICE_STATS`, gated with the hardware tree it details). Its
compositor's layer limits and per-layer opacity fill the compositing-path
block; its scan-out mode, its interval utilisation, and the memory it owns
fill the device block. Utilisation is a delta over the sample's own interval,
never the service lifetime's average, so a first sample states no share; a
device with no memory of its own says so in words, because that is a different
statement from none being free. A **per-engine** breakdown still has no
producer — no display driver reports its engines separately — so that row
carries the honest unmeasured mark rather than one device's occupancy dressed
as an engine's.

**A device's commands are labelled, not glyphed**, and almost none has an
endpoint. Of the commands the panes offer, only "sort tasks by *resource*" is
one this service can carry out: it is a view transition onto the Tasks table,
ordered by that device's own cost, so a busy device is traced to the tasks
sitting on it. Every other command is drawn *plainly disabled* for want of an
endpoint rather than marked for authority — acquiring a capability would not
make an absent endpoint appear.

**Selecting a device performs no I/O.** The rail's selection changes which
pane is drawn from state the sampler has already delivered: it issues no
query, opens no store and waits on nothing, and a pane with no sample yet
reads unavailable rather than blocking for one.

**When the window is too narrow to seat the rail, its *route* moves into the
band** as a `ComboBox` naming the current device, whose list is the same
device set the rail held. Losing the rail must not lose a pane, so what
replaces it is a control rather than an omission.

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

### What is deliberately empty, and why

- **Background jobs** — there is no job registry anywhere in the OS to
  enumerate, so no section shows one. It returns as a `Jobs` tab and a
  `Type` column on Tasks the day a registry lands, not as a section.
- **Services** — the System Information API (`lib/abi/src/sysinfo.rs`) has
  no service-enumeration query, so nothing claims to list them.
- **A graphics device's engine utilisation and video memory** — the hardware
  tree names the device, but no query publishes its per-engine busy time or
  its memory, so those facts are marked. The pane leads with the compositor's
  measured frame cost precisely so a machine with no GPU still reads usefully.
- **Per-task network bytes** — no per-process socket accounting exists
  anywhere in the system; attribution belongs to the network service, which
  owns the sockets. The interface pane states that in words.
- **Temperature, AC and battery** — no sensor or power-supply interface
  exists, and no driver to serve one.
- **In-panel system actions** — the machine's power transitions are not
  rows *in this window*: they live in the taskbar's quick-actions menu,
  where the user confirms them, and reach this service as the `Power`
  command below. Session lock is the desktop session's own surface (it
  keeps the session running behind it), never this service's.
- **An accelerator pane** — there is no accelerator device class for
  discovery to report, so the rail grows no `Accelerators` group and the pane
  does not exist. It arrives with the driver class, not as a greyed-out
  teaser.

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
| Resource *Sort tasks by …* | resolved inside the widget: shows the Tasks table ordered by what that device costs, so a busy device is traced to the tasks on it |
| Every other resource command | nothing: no endpoint exists to drive a reclaim, a scrub, a trim, an unmount, a lease renewal or a clipboard, which is why each is drawn plainly disabled |
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
armed. Its folding event stream (`tairix_window::WindowEvents` over an
`EventMailbox` keyed to the identity the create reply attested) is created
and dropped with the window for the same reason; the mailbox itself is the
process's and outlives any one window, so an event still queued from a
window that has gone is dropped on the id it names rather than applied to
its successor. The periodic re-sample is the documented polling fallback: the
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
