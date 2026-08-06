# NEW-SWITCHBOARD — the Switchboard surface

Binding under `AGENTS.md`. This plan fixes the Switchboard window's
information architecture, the controls it is built from, and which of its
readings are real measurements. The monitor *service* behind the window — its
sampling cadence, tray-summary contract, capability sizing and lifecycle — is
`plans/NEW-TASKBAR.md` T10–T12 and is unchanged by this plan.

The reference designs are `plans/switchboard1.png` (Tasks),
`plans/switchboard2.png` (Background), `plans/switchboard3.png` (System) and
`plans/switchboard4.png` (Recovery).

## S1 — Where the composition lives — done

The Switchboard *screen* is application-specific composition, so it lives in
the application: `userland/gui/switchboard/src/view/`, one module per
section over a shared frame module. `lib/controls` holds only controls that
any surface may reuse. There is no `lib/controls::switchboard`.

The controls' shared heavier-contrast test fixture is reachable outside the
crate through the `test-support` feature (`tairix_controls::testkit`), so the
view's own render tests exercise the same two contrast axes as the controls
without a second copy of the fixture.

## S2 — Chrome — done

The window is a `WindowFrame` with the standard `TitleBar` and window
commands; the client viewport is the only application region.

Below the title bar sits the **location band**:

- a `Breadcrumb` on the left reading `Switchboard › <section>`. Its trailing
  crumb is the current location, which a breadcrumb never activates, so the
  leading crumb is the route: activating it opens the section list.
- the active section's own **band summary**, if it declares one: a handful of
  counts describing the whole list at a glance, seated between the trail and
  the command. Tasks' four census tiles live here.
- a section-list `IconButton` (`IconKind::ListMenu`) at the trailing end,
  which opens that same `Menu` of the six sections. The section on show is
  marked with `Menu::set_current` *and* the item's `SelectionState::Selected`
  — the pair `ComboBox` already marks its own choice with, not a second
  convention.

The band's three regions are resolved once by `frame::resolve_band`, which
both the paint and the hit test read, so a press can never land on a control
drawn elsewhere. The band's *height* belongs to the section on show
(`SectionAnatomy::band_height`): one control height at rest, or as much more
as its summary needs, so a section with no census pays for none. A band too
narrow to seat the summary beside a whole trail drops the summary rather than
abbreviating the trail — the reader's own location outranks a census the
table still states.

There is no global tab strip: the band is the whole section switcher, and
both routes run the one `select_section_index` transition, so the trail, the
content and the per-section scroll offset cannot disagree. The band is a
Tab-cycle focus region, so a section is reachable without a pointer: Space or
Enter opens the list, Up/Down walk it, Enter shows the section under the
cursor, Escape closes it unchanged. The list is modal while open, which is
also what keeps it and the Tasks Group popup mutually exclusive.

There is **no permanent resource band**: a resource reading belongs to the
section that is about it, so the readings are the System section's header
tiles and Tasks' own census tiles. A strip of meters above every section
would state the same numbers twice and steal height from the section a reader
actually asked for. Tasks' census rides the location band precisely because
it is a census of *that* section, not of the machine.

Content taller than the section's primary column is governed by the one
shared vertical `ScrollBar`, with the `ResizeGrabber` at its junction.

## S3 — The section frame — done

Every section is the same anatomy, resolved once in `view/frame.rs` and drawn
into by all six, so no section restates the geometry:

```
 sidebar? |            header?                                  |
          |  primary   |  detail?   |  impact?   |  rail?       |
          |            footer?                                  |
```

- `sidebar` — a leading navigation column (System's vertical `Tabs`).
- `header` — the section's own instruments and filters.
- `primary` — the master list or table. Always present, and the only region
  the shared `ScrollBar` governs.
- `detail` — the pane describing the primary's selected item.
- `impact` — the narrow stack of readings *about* the subject the detail pane
  describes (Recovery's per-task CPU, memory, disk and network).
- `rail` — the trailing `ActionRail` of commands for that selected item.
- `footer` — the section's status line and its section-wide controls.

Each section declares the regions it wants in *logical* lengths as a
`SectionAnatomy`; `resolve_section_frame` resolves them against the client
and, when the window is too narrow to seat them all, drops the optional ones
in one fixed order — `detail`, then `impact`, then `rail`, then `sidebar` —
so `primary` always survives and the drop order is a property of the frame
rather than a per-section improvisation. A section that declares no header,
footer, detail, impact or rail simply gets none.

**`primary` has a declared floor, and shedding honours it.** A region is shed
when `primary` would fall below `SectionAnatomy::primary_floor`, not merely
when it would reach zero: a section whose rows carry inline commands declares
how many (`primary_row_commands`) and the frame turns that count into the
width the strip genuinely needs — the commands, the gap keeping them off the
row's text, and the row's trailing inset. Activities declares four, which is
why a narrow window sheds its detail pane instead of pushing a group's
commands off its own row. The count is declared rather than the width because
the width needs the theme and the live `Scale`; `primary_floor` and the row
splitter that lays the buttons out share one arithmetic
(`frame::action_button_width` / `frame::row_commands_width`), so a declared
floor cannot drift from the strip a row draws. A content narrower than the
floor itself has nothing left to shed and simply gets the whole width.

`panel.rs`'s `MIN_WIN_WIDTH`/`MIN_WIN_HEIGHT` stay the panel's *readability*
floor rather than becoming a derived value: the floors need the theme's
control metrics and the live `Scale`, so they cannot produce a `const`. The
two are instead *tied* to the anatomies by a test asserting the minimum window
keeps every section's `primary` at its declared floor and keeps every sidebar
and rail any section asks for, so a section that outgrows the floor fails the
suite instead of clipping a command. `MIN_WIN_WIDTH` is **not** raised to the
width at which every optional column fits: shedding an optional column is the
drop order working as designed, and enlarging the window until the shedding
stops would be mitigation, not a floor.

An `ActionRail`'s column is one width wherever it appears
(`frame::ACTION_RAIL_WIDTH`), so a reader who learns where the commands sit
in one section finds them in the same place in the next.

Each section is a struct in its own module owning its view models, its
retained controls, its cursor and its section-private overlays (Tasks' Group
popup, Activities' inline rename), reached through one `SectionView` dispatch
(`anatomy`, `adopt`, `render`, `on_pointer`, the content/action cursors,
`activate_focused`, and the primary column's scroll extent). `view/mod.rs`
therefore holds the window frame, the chrome, the scroll model, the region
focus policy and the one `match` that names the active section — never a
second copy of a section's behaviour.

A section with a *selection* re-resolves it on every `adopt` through the one
shared rule (`view::resolve_selection`): the subject's own stable identity is
remembered — Recovery's `ProcId`, Background's job name — and re-found in the
fresh list, so the selection survives a reorder and drops only when the
subject genuinely goes. A row number would silently re-point at a different
subject the moment one above it left. Sections with no selection simply clamp
their cursor into the freshly derived content.

A section whose master list is `Card`s — Pressure, Recovery, Background —
*makes* that selection through one shared walk (`view::select_pressed_card`):
it offers the pointer event to the card in each visible slot and reports
whichever one answered along with its own `CardAction`. A body press
(`CardAction::Pressed`) selects the card, so pressing a card opens its detail;
a footer click (`CardAction::FooterActivated`, only Pressure's cards carry
one) selects it *and* resolves that button's command, so a command can never
act on a subject other than the card that offered it. A card that is not
actionable reports nothing and selects nothing. One walk rather than one per
section is what keeps the three from drifting apart as they evolve.

## S4 — The six sections' interiors — done

**Selection must survive a refresh.** A master/detail section is unusable if
the detail pane changes object every time a sample lands, so the first section
to grow a `detail` pane brings the identity rule with it: every view model
carries the model's stable identity for its item and a section re-resolves its
selection against that identity after `adopt`, dropping it only when the item
genuinely went away. The view never interprets the identity; it only compares.

### Tasks (`plans/switchboard1.png`) — done

- **band summary** — four census `MetricTile`s (Processes, Jobs, Services,
  Alerts) in the location band (S2), each *plated* and carrying the glyph of
  the thing it counts, tinted by a `PressureKind` used as an identity colour
  rather than as a claim that a resource is strained. `CENSUS` is their one
  declaration: the tiles are built from it and the room the band is asked for
  is measured from it, so the band can never seat a different number of tiles
  than the section draws.
- **header** — the filter `Tabs` strip (All, Processes, Jobs, Services,
  Faults) on its own row, whose labels carry each filter's count and which
  filters the table rather than switching section; then a `SearchField`
  matching on task name, case-insensitively, over its own full-width row
  beneath it. *Which kind* of task and *which* task are separate questions, so
  each gets a row. Every tile and every tab counts adopted rows through the
  *same* predicate, so a tile and its tab can never state different numbers.
- **primary** — a sortable `TableHeader` over `TableRow`s: Task (its
  `IconKind` and name), Type, State, Activity (a per-task CPU `Chart`
  sparkline), CPU, Memory, Disk, Network, Last active. Every column is a
  *reading* about the task; what may be done to it is the rail's business.
  Sorting is the header's, applied over the filtered rows and stable, so rows
  it cannot separate keep the order the sample reported. `COLUMN_WEIGHTS` is
  the one definition of the column geometry: the heading, the cells and the
  sparkline's own rect (`TableRow::cell_rects`) all read it.
- **rail** — `ACTIONS` for the *selected* task, in the standard trailing
  `ActionRail` seated in a `Panel` that captions it, so the commands stay
  anchored while the rows scroll beneath them. `RAIL_COMMANDS` declares them
  in reading order — Switch to, Reveal window, Pause, Resume, Lower priority,
  Open logs, Group…, Force quit — each with the glyph that says it without
  words. Force quit is `ControlRole::Destructive`, so it wears the danger rim
  and sits last, where a mis-aimed press is least likely to land. Every item
  renders its own verdict: permitted, plainly disabled where the task's state
  rules it out, or the Authority Mark where the caller lacks the authority.
  With nothing selected the rail holds no commands at all rather than a column
  of refusals, and the plate keeps its place either way.
- **footer** — the shown/total count and the Auto-refresh `Toggle` beneath the
  table, and the grouping `ComboBox` (ungrouped, by type, by activity — an
  arrangement of the same rows, not a new query) beneath the rail, so each
  control sits under what it governs. Auto-refresh holds the table on the
  sample the reader is reading rather than moving it under them.
- **cursor** — the section's content cursor spans header stops, then rows,
  then the rail's commands, then footer stops, so every control is
  keyboard-reachable without hanging off a row that a filter could remove.
  `SectionView::focus_row` maps a cursor stop back to the row it names (`None`
  for the chrome bands and the anchored rail), which keeps the scroll-into-view
  arithmetic in `view/mod.rs` as the one definition; `item_count`/`list_info`
  still mean the filtered, sorted rows alone.

**The commands act on the selection, not on a row.** A `ProcId` — the task's
stable, never-reused instance identity — is what the selection remembers, so
it survives a refresh, a re-filter and a re-sort rather than following
whichever row slid into its place, and it drops only when the task genuinely
goes. A table with rows always has one selected (the shared
`view::resolve_selection` rule), so the commands always have a subject. This
is what lets the rail state a task's whole repertoire instead of the one or
two buttons a row's trailing cell could hold.

`TaskAuthority` carries one verdict per command, reached in `model.rs` where
the caller's authority *and* the task's lifecycle state are both known:
signalling needs `PROC_CONTROL`, and with it the state still rules out what
makes no sense (pausing a stopped task, resuming a running one, anything at
all for a task that has already exited). `apply_action` re-checks that same
verdict before acting, so a command drawn as denied or disabled can never be
carried out by an unexpected report of it. `TaskControl::Reveal` is the same
request of the session as `Switch` — raising the window is how this system
shows a reader where it is, and there is no separate highlight-without-raising
interface to invent one for. `TaskControl::OpenLogs` is permanently disabled:
no capability-gated query for a task's own log entries exists (S6), so the
command states its absence rather than pretending to work.

**A row wears no activity seam.** An activity in a control's state paints a
Heat Seam along its whole lower edge, which under a table row reads as an
orange rule beneath every working task rather than as a reading about one. A
task's activity is shown in the Activity column instead, as the sparkline the
heading promises; the row's state carries only its pressure (a Pressure Rail
in the leading gutter) and its recovery posture (a Signal Bead).

**Type** names what the row *is*, not what it is for: `TaskKind::Process` is
what the process list reports, and `Job`/`Service` are the kinds a job
registry and the service manager will contribute (S6) — which is why their
census tiles and tabs honestly read zero today rather than guessing.

Three filters the reference boards sketch are deliberately **not** spelled,
because no reading backs them: *Background* (the process list carries no
foreground/background signal), *Recent* (there is no last-active interface,
S6), and *Hung*, which is folded into *Faults* — one filter over the shared
`process_recovery` classifier that already resolves both stopped and
seat-reported-unresponsive tasks, so the tab, the rows' Signal Beads and the
Recovery section can never disagree about which tasks are faulted.

**Disk** is a real measurement: `TaskMeters` (`model.rs`) deltas each task's
`io_bytes_read + io_bytes_written` against its own previous reading over
`Sample::elapsed_ns`. A first sample, a task first seen this sample, and an
unmeasured interval each yield no rate (a cumulative total is not a rate); a
counter that did not move over a real interval is a genuine `0`. **Activity**
plots the same store's bounded per-task CPU ring (`TASK_HISTORY_LEN`, the
window the resource charts plot), keyed by `ProcId` so a recycled pid cannot
inherit a dead task's history, and rebuilt from each sample so an exited task
leaks neither its history nor its counters. **Network** and **Last active**
have no interface at all (S6) and render the explicit unmeasured mark.

### Background (`plans/switchboard2.png`) — done

- **primary** — a `Card` per job (name, what it is doing, its progress as the
  card's Heat Seam); an empty list renders the shared absence line plus a
  sentence naming that no registry exists.
- **detail** — the job's name and percentage, a throughput `Chart`, two
  columns of `FactList`, then a `Timeline` of what the model can attest.
- **rail** — `JOB ACTIONS` (Pause, Cancel) for the selected job, reached
  through the shared safe keyboard route so a refused command refuses the
  keyboard too.
- **footer** — the Auto-throttle `Toggle`, *plainly disabled* rather than
  wearing the Authority Mark: the caller's authority is not what is missing.

No job registry exists (S6), so the list is empty and states why; the
anatomy is what a registry fills. Selection is remembered by the job's own
name — the only identity a job has until a registry issues one — through the
shared selection rule, so it survives a reorder. The two `FactList` columns
report what the model carries (the job and what it is doing; whether each
command is permitted); the fields the concept board sketches (source,
destination, transfer speed, files processed, bytes copied) are what a
registry would supply and are not invented here.

### Pressure — done

- **primary** — one `Card` per flagged resource, each keeping its own relief
  verdicts (`Ready`, `DisabledByState`, the Authority Mark) and the
  `Show tasks` transition into Tasks focused on the culprit. There is no rail:
  a cause's relief lives in the card that offers it.
- **detail** — the cause's `FactList`: `Resource`, `Pressure` (the amount, from
  the machine-wide CPU busy share or the memory band's own measured share),
  `In band` (how long it has stood there), and `Relief`.

Every figure is a reading the model carries; the card's prose is never
re-parsed to recover a number from it. `Relief` names the model's own
recommended command and, where this session cannot take it, why not — `not
permitted` for want of the capability, `not available in this state` otherwise
— so a refused relief is stated rather than hidden while the command itself
still fails closed at its button, to the keyboard as to the pointer. A cause
recommending nothing says so instead of volunteering another command.

A band's age has no interface behind it (nothing timestamps a band change), so
`PressureClock` tracks when each resource entered its band — clocked off the
monotonic uptime reading, exactly as `FaultClock` ages a fault, sharing one
`elapsed_since` definition — and forgets it the sample the band eases, so a
resource that comes back under pressure is timed from its new band. With no
uptime reading the age reads unmeasured, never a fabricated zero.

Selection is remembered by the *resource* (`PressureKind`) through the shared
selection rule: the service raises at most one cause per resource, so that is
the cause's stable identity. It survives a reorder and drops when the resource
eases.

### Activities — done

- **primary** — an activity header row per group with its member rows
  beneath, the header carrying the group's four commands (Switch,
  Pause-or-Resume, Rename, Close) and the inline rename.
- **detail** — the selected activity's combined readings and one line per
  member.

The group's `CPU`, `Memory` and `Disk` are sums of its **joined** members' own
measured readings — no per-group accounting exists to read — through one
`member_total` helper: one unread part makes the whole total absent (a total
that skipped a member would understate the group while reading as a
measurement), and a group with no joined member has no total rather than a
nought claiming it costs nothing. `Network` is always unmeasured: there is no
per-task network accounting to total. A member line states plainly that it is
not running when it has exited, and wears the unmeasured mark when it is
running but its own figure was not read.

Selection is remembered by the group's stable id, and pressing any part of a
header — its name or any of its four commands — selects that group, so the
pane describes the group the reader just acted on. The in-flight rename is
re-located by the same id, never by row.

### Recovery (`plans/switchboard4.png`) — done

- **primary** — a `Card` per fault: name, what happened, and how long ago.
- **detail** — the fault's identity, a `StatusPill` naming its impact, a
  `FactList` (status, age, recommendation), then a `Tabs` strip over three
  pages: Timeline (the marks this service observed), Crash Snapshot (the
  kernel's `CRASH_RECORD` — fault class, distance from its anchor, access
  direction, owning uid/gid, `pc`/`sp`/`fp`, every named register and every
  backtrace frame), and Logs (no interface, stated).
- **impact** — a stack of unplated `MetricTile`s for the faulting task's CPU,
  memory, disk and network; network is always unmeasured (no query reports a
  process's network use).
- **rail** — `RECOVERY ACTIONS` for the selected fault, carrying only the
  commands this service backs: Restart, and Force with its confirmation
  posture or the Authority Mark. Recovery, Background and System command one
  *selected* subject, which is what an `ActionRail` is; Tasks commands each
  row, which is why its commands are a column.
- **footer** — the resolved-fault count, carried in the model because only
  something folding one sample into the next can see a fault clear.

The crash record is matched to its fault by `ProcId` and nothing else: a
numeric pid is reused, so matching on one could attribute a dead task's crash
to a live task that inherited its number. A fault with no record says so
plainly and does *not* wear the unmeasured mark — a stopped or unresponsive
task has faulted without ever raising a user fault.

A fault's age has no interface behind it (no state-change timestamp exists
anywhere in the API), so the service tracks when it first saw each task
faulted — keyed by `ProcId`, clocked off the monotonic uptime reading, pruned
the first sample the fault clears, and counting each pruned entry as the
resolved tally. With no uptime reading the age reads unmeasured, never a
fabricated zero.

Selection is remembered by `ProcId` through the shared selection rule, so it
survives a refresh that reorders the list and drops only when the fault
genuinely goes.

### System (`plans/switchboard3.png`) — done

- **sidebar** — a vertical `Tabs` column: Overview, Resources, Storage,
  Network, Session, Permissions, Services, Power. It selects the page, not
  the section, so it is the sidebar and the location band stays the one
  section switcher.
- **header** — four `MetricTile`s: CPU and Network plot a `Chart` trend
  (a rate has no fixed ceiling to fill a bar against), Memory and Disk fill
  a track (each is a fraction of a measured whole).
- **primary** — the selected page: Overview is the machine's `FactList`, the
  Active Services statement and the permissions summary; Resources is the
  per-core load and the memory/kernel-heap detail; Storage is the mounts with
  their capacity *and* health; Network is the per-interface facts, link state,
  addresses and rates; Session is the seats and the logged-in census;
  Permissions is the authority summary with the resource limits and their
  live usage; Services and Power state what has no interface (S6).
- **rail** — `SYSTEM ACTIONS`, seated in a `Panel` because `ActionRail`
  carries no caption of its own.

Every page compiles to one ordered `PageLine` vocabulary — heading, fact,
absence — so the section has a single layout, scroll range and paint loop
rather than eight of each.

Every action emits a typed view action the service authorises and applies;
the view performs no privileged work. A refusal names its own kind: an
action refused for want of a capability wears the Authority Mark, because
acquiring the authority would make it available, while an action with no
endpoint behind it is plainly disabled.

The cursor's stops are the eight pages and then the rail's buttons, so
Up/Down walks the sidebar as a reader expects of a vertical list and
Enter/Space commits. The `Tabs` control's own vertical navigation is
deliberately not fed the same keys, which would give them two meanings.

`Section::Jobs` is this plan's **Background** and `Section::System` is its
**System**; the wire-level `CommandSection::System` carries the same name.

## S5 — What the service samples — done

Beyond the process list (`SELF_PROCESS_LIST`/`GLOBAL_PROCESS_LIST`),
`CPU_TIME_STATS` and `MEMORY_PRESSURE`, the sampler reads every query below.
Each reading is its own sampled field, capability-gated at the query,
degrading exactly the field it backs and nothing else. A field the sampler
could not read carries an `Absence` saying *which* — a scope the caller does
not hold reads `not permitted`, a query that simply did not answer reads
`unavailable` — because those are different statements to a reader:

| Query | What it backs |
|---|---|
| `SYSTEM_IDENTITY` | hostname, machine id, OS version |
| `UPTIME` | uptime, boot time |
| `LOAD_AVERAGE` | load average, logged-in census |
| `CPU_INFO` | core count, model, live frequency |
| `CPU_LOAD` | per-core load |
| `KERNEL_MEMORY_STATS` | kernel heap/slab detail |
| `MOUNT_LIST` | the Storage page's mounts, and each volume's used/total space from its `VolumeStats` block counts |
| `VOLUME_IO_HEALTH` | a volume's health state |
| `NET_INTERFACE_FACTS`/`_STATE`/`_RATES` | the Network page and the network tile |
| `SEAT_LIST` | the Session page |
| `RESOURCE_LIMITS` | the caller's limits and live usage |
| `CRASH_RECORD` | Recovery's Crash Snapshot page |

The process record's I/O bytes are already sampled, and back the Tasks table's
Disk column (S4) and the Recovery impact stack.

Per-task CPU history for the Activity sparkline is the service's own: it keeps
a bounded per-task ring of the CPU permille it already measures, keyed by the
task's stable identity, so a sparkline plots measurements rather than a shape.

## S6 — Readings with no interface yet

These render an honest unmeasured mark (`MeterValue::Unmeasured`, an
unmeasured cell, or a page stating its reason), never a fabricated number.
An empty list is *not* such a mark: it reads as "none", so a reading with no
interface states its absence in words instead.
Each needs its own interface before the surface can fill in; the layout
already has the slot.

| Reading | What is missing |
|---|---|
| per-task network bytes | no per-process socket accounting; attribution would belong to the userland network service |
| per-task uptime / last-active | `ProcessRecord` carries no creation timestamp |
| service list with state/CPU/memory | no service manager exists (`plans/NEW-SERVICEMANAGER.md`) |
| background jobs with progress | no job registry exists anywhere in the system |
| temperature, AC/battery | no sensor or power-supply interface, and no driver to serve one |
| log reading | the journal has an ingress path but no capability-gated read query (`plans/SYSLOG.md`) |

Per-task **disk** bytes are measured: the kernel accounts the bytes each
process's own file reads and writes actually transfer, reported on
`ProcessRecord`, and the view derives a rate from the delta between samples.

A fault's **age** has no query behind it either, but it is not left
unmeasured: the service keeps when it first saw each task faulted, keyed by
`ProcId` and clocked off the monotonic uptime reading, and prunes the entry
when the fault clears. That is an observation the service can attest, so it
is reported as one — and it reads unmeasured only when there is no uptime
reading to measure against.

## S7 — Controls this surface added to `lib/controls` — done

Generic, reusable, and complete on landing (every state, both themes, the
heavier-contrast path, pointer and keyboard where interactive):

- `nav::Breadcrumb` — a location trail whose trailing crumb is the current
  location and is not activatable, eliding oldest-first with an activatable
  ellipsis so the current location is never dropped.
- `metric::MetricTile` — an optional identity icon, a label, a large reading
  with a quiet unit, an optional detail line, and an optional instrument
  (`MetricInstrument`: a proportional track reusing `MeterValue`, so an
  unmeasurable resource shows a bare groove rather than a fabricated zero, or
  a `Chart` trend). Its `MetricLayout` chooses the stacked form that fills a
  column of its own or the inline form that puts label and reading on one
  line; an unplated tile draws no plate, so a stack of readings shares one
  container's surface instead of nesting plates.
- `metric::StatusPill` — a compact capsule naming a state, toned by
  `SignalRole`.
- `record::FactList` — right-aligned key/value readouts where the value
  keeps its room and the label truncates first.
- `record::Timeline` — a spine spanning only first to last mark, shape-coded
  marks, and a stamp column sized to the widest stamp.
- `rail::ActionRail` — the vertical counterpart of `Toolbar`, composing
  `Button`s so plate, role, disabled and denied rendering are not restated.
- `collection::TableHeader` — sortable column titles sharing the row
  family's one column-width model, reporting a sort the owner commits.
- `tabs::TabsOrientation` — a vertical orientation of the existing strip, so
  a sidebar is not a second selection control.

The measured-track geometry — groove, proportional tinted fill, pressure
outline — has one definition in `controls::paint`, which `MetricTile`'s
`Track` instrument draws through; it is the only reading-with-a-track in the
design language.
