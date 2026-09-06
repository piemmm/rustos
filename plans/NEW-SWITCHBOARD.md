# NEW-SWITCHBOARD — the Switchboard surface

Binding under `AGENTS.md`. This plan fixes the Switchboard window's
information architecture, the controls it is built from, the readings it
draws, and the interfaces those readings still need. It supersedes the
six-section design entirely: the section set, the resource surface and the
wire `CommandSection` all change, and S10 lists what that deletes.

The monitor *service* behind the window — its staged sampling cadence,
tray-summary contract, capability sizing and lifecycle — is
`plans/NEW-TASKBAR.md` T10–T12. This plan adds readings to its sample set
(S5) and moves one query between cadence tiers; nothing else about the
service changes.

## Ledger

Every work item this plan calls for, what it waits on, and where it is
specified. A task is `done` only when its tests and docs landed with it.
Nothing here is optional: an item dropped is a reading the surface then has to
lie about.

| # | Task | Depends on | Spec | Status |
|---|---|---|---|---|
| **A1** | `Section` and wire `CommandSection` carry exactly `Tasks`, `Resources`, `Recovery`; discriminants renumbered with no reserved gap; `map_section` and its exhaustive table shrink | — | S4 | done |
| **F1** | Resources' `SectionAnatomy`: the device rail as `sidebar`, the pane as `primary`, and the shed route replacing the rail with a band `ComboBox` | A1 | S3 | done |
| **C1** | `chart::Chart` gains an optional opposing series, mirrored below a drawn midline and tinted by its own `PressureKind` | — | S7 | done |
| **C2** | `metric::CompositionBar` — named proportional segments of a measured whole, with its key; segments that do not sum to the whole are a construction error | — | S7 | done |
| **C3** | vertical `tabs::Tabs` gains group headings and per-item reading + bounded trend | — | S7 | done |
| **P1** | `PressureKind::{Gpu, Accelerator}` and `gpu_pressure` / `accelerator_pressure` in both built-in themes | — | S9 | done |
| **D1** | `drivers/accelerator/` class with its trait in `lib/abi/src/driver/accelerator.rs`, bound through the ordinary discovery-match path | — | S8 | planned |
| **Q1** | `VOLUME_IO_STATS` — ungated, per volume: bytes, ops, `busy_ns`, read/write `wait_ns` | `plans/FIX-IO.md` per-device counters | S8 | planned |
| **Q2** | `VOLUME_IO_QUEUE` — `CAP_SYSINFO_KERNEL`, audited: `in_flight`, queue depth sum + samples, the class budget in force | `plans/FIX-IO.md` per-device counters | S8 | planned |
| **Q3** | `GPU_DEVICE_STATS` — `CAP_SYSINFO_HW`: per-engine `busy_ns`/`idle_ns`, device memory, and the device's `AccelCaps` | `plans/FIX-DISPLAY-ACCELERATION.md` accel path | S8 | planned |
| **Q4** | `ACCEL_DEVICE_STATS` — `CAP_SYSINFO_HW`: `busy_ns`/`idle_ns`, device memory, `in_flight` | D1 | S8 | planned |
| **M1** | `CPU_INFO` moves `Cadence::Static` → `EverySample`, so the live clock is a live reading | — | S5 | done |
| **M2** | Q1–Q4 enter the cadence table on `EverySample`, each degrading only the field it backs | Q1, Q2, Q3, Q4 | S5 | planned |
| **V1** | `view/resources/`: the shared pane frame and the grouped per-device rail, its length discovered rather than declared | A1, F1, C3 | S4 | done |
| **V2** | CPU pane — hero busy trace and the per-core grid (trace, busy share, live clock, performance class) | V1, M1 | S4 | done |
| **V3** | Memory pane — composition bar, the pressure banner with its recommended relief and refusal kinds, the bounded-cache reclaim ledger | V1, C2 | S4 | done |
| **V4** | Volume pane — capacity, medium and the bucketed health block; the service-and-queue block fills from Q1/Q2 | V1, C1, Q1, Q2 | S4 | done |
| **V5** | Interface pane — duplex rate trace over its stated window, link, counters, stack | V1, C1 | S4 | done |
| **V6** | Graphics pane — the frame-work breakdown, the compositing path, the device; self-report suppression preserved | V1, P1, Q3 | S4 | done |
| **V7** | Accelerator pane — reports what discovery knows (node, class, match keys, unbound); readings fill from Q4 | V1, P1, D1, Q4 | S4 | planned |
| **V8** | Machine group panes — identity and uptime, seats and census, authority with limits and live usage | V1 | S4 | done |
| **V9** | Tasks amendments — Owner and Core columns, the owner + fault filters, the census tiles | A1 | S4 | done |
| **V10** | Top-consumers block on the CPU, Memory and volume panes, stating that a sum of tasks is not the device's total | V2, V3, V4 | S4 | done |
| **X1** | Delete `view/{background,pressure,activities}.rs` and their tests; `PressureClock` and the cause model move to V3's banner, the group model to V9's grouping | V3, V9 | S10 | done |
| **X2** | Delete `view/{system,system_data}.rs`, the `PageLine` vocabulary and `SystemReport`'s `cores`/`memory`/`compositor` fact vectors | V2, V3, V4, V5, V6, V8 | S10 | done |
| **X3** | Re-point `view/tasks.rs`'s three board citations at `plans/switchboard/01-tasks.png` | V9 | S10 | done |
| **X4** | Rewrite `docs/src/desktop/switchboard.md` — it describes the section set | V1–V10 | S10 | done |
| **R1** | `plans/NEW-TASKBAR.md`: re-point the tray capsule, the long-press route and T13's quick-actions menu at the surviving sections | A1 | S11 | done |
| **R2** | `plans/GUI-CONTROLS-DESIGN.md`: enter C1–C3 in the control families with their settle-point and damage obligations | C1, C2, C3 | S11 | done |
| **Z1** | Responsiveness verticals — selection performs no I/O, a paint reads nothing, an input burst yields one paint, a fresh sample damages only what moved | V1–V8 | S12 | planned |
| — | Where the composition lives, and the `testkit` contrast fixture | — | S1 | done |
| — | The location band: breadcrumb, band summary slot, section list, one `select_section_index` transition, no permanent resource band | — | S2 | done |
| — | The section frame resolver, the fixed drop order, `primary_floor` and the row-command arithmetic | — | S3 | done |
| — | The shared selection-identity rule, `select_pressed_card`, `PressureClock`, `FaultClock`, the `ProcId` crash match | — | S4 | done |
| — | Recovery's interior: fault cards, detail tabs, impact stack, action rail, resolved tally | — | S4 | done |
| — | The eight controls this surface already contributed to `lib/controls` | — | S7 | done |

**A1 through X4 are one change, and it is a large one.** A1 deletes four
sections, so every pane that absorbs their readings has to exist in the same
tree, and the four sections' own test modules go with them — the bulk of the
change is `view/{background,pressure,activities,system,system_data}.rs` and
their tests plus the references to `Section::{Jobs,Pressure,Activities,System}`,
`SystemReport`, `SystemPage`, `JobSummary`, `ActivitySummary` and
`PressureCause` in `view/mod_tests.rs`, `view/test_support.rs`, `panel_tests.rs`
and `model_tests.rs`. Plan it as one change over several sittings against a
branch, not as one sitting: a partially built Resources section cannot be
landed, because A1 leaves the surface with no home for the readings it deletes.

The reference storyboard is `plans/switchboard/`:

| Board | Shows |
|---|---|
| `00-map.png` | the three sections, and every reading traced to its query |
| `01-tasks.png` | Tasks — the table and its per-task trace |
| `02-cpu.png` | Resources → CPU, with the per-core grid |
| `03-memory.png` | Resources → Memory, with the pressure banner |
| `04-disk.png` | Resources → a volume: health measured, rates pending |
| `05-network.png` | Resources → an interface |
| `06-graphics.png` | Resources → Graphics (compositor work, then the device) |
| `07-accelerator.png` | Resources → the accelerator slot, and what it costs |
| `08-recovery.png` | Recovery |
| `09-theme-and-shed.png` | the light theme, and the narrow window's shed order |

## S1 — Where the composition lives — done

The Switchboard *screen* is application-specific composition, so it lives in
the application: `userland/gui/switchboard/src/view/`, one module per
section over a shared frame module. `lib/controls` holds only controls any
surface may reuse. There is no `lib/controls::switchboard`.

The controls' heavier-contrast test fixture is reachable outside the crate
through the `test-support` feature (`tairix_controls::testkit`), so the
view's render tests exercise the same two contrast axes as the controls
without a second copy of the fixture.

Resources is large enough to want its own directory: `view/resources/`, one
module per pane over a shared pane frame, reached through the one
`SectionView` dispatch like any other section. A pane is not a section.

## S2 — Chrome: the location band — done, section set amended

The window is decorated **server-side** by the window manager (see
`plans/COMPOSITOR-WORK.md`): title bar, window commands, frame and resize
grabber are the compositor's, drawn around the client. Switchboard draws no
chrome of its own — its content is the whole client, starting with the
location band — and resizes only by re-mapping its region on
`WindowEvent::Resized`. Those arrive one per pointer sample of a resize
grab, so they are read through the shared folding stream
(`tairix_window::WindowEvents`) and a whole drag costs one re-map.

At the top of the client sits the **location band**:

- a `Breadcrumb` on the left reading `Switchboard › <section>`. Its trailing
  crumb is the current location, which a breadcrumb never activates, so the
  leading crumb is the route: activating it opens the section list.
- the active section's own **band summary**, if it declares one, seated
  between the trail and the command. Tasks' four census tiles live here;
  Resources and Recovery declare none.
- a section-list `IconButton` (`IconKind::ListMenu`) at the trailing end,
  opening a `Menu` of the sections. The section on show is marked with
  `Menu::set_current` *and* the item's `SelectionState::Selected` — the pair
  `ComboBox` already marks its own choice with, not a second convention.

The band's three regions are resolved once by `frame::resolve_band`, which
both the paint and the hit test read, so a press can never land on a control
drawn elsewhere. The band's *height* belongs to the section on show
(`SectionAnatomy::band_height`): one control height at rest, or as much more
as its summary needs. A band too narrow to seat the summary beside a whole
trail drops the summary rather than abbreviating the trail — the reader's own
location outranks a census the table still states.

There is no global tab strip: the band is the whole section switcher, and
both routes run the one `select_section_index` transition, so the trail, the
content and the per-section scroll offset cannot disagree. The band is a
Tab-cycle focus region, so a section is reachable without a pointer.

**There is still no permanent resource band.** A resource reading belongs to
the surface that is *about* it. Resources' readings are its own rail and pane
headers; Tasks' census rides the band precisely because it is a census of
that section. A strip of meters above every section would state the same
numbers twice and steal height from the section a reader asked for.

Content taller than a section's primary column is governed by the one shared
vertical `ScrollBar`.

## S3 — The section frame — done, Resources anatomy added

Every section is the same anatomy, resolved once in `view/frame.rs` and drawn
into by all of them, so no section restates the geometry:

```
 sidebar? |            header?                                  |
          |  primary   |  detail?   |  impact?   |  rail?       |
          |            footer?                                  |
```

- `sidebar` — a leading navigation column (Resources' device rail).
- `header` — the section's own instruments and filters.
- `primary` — the master list, table or pane. Always present, and the only
  region the shared `ScrollBar` governs.
- `detail` — the pane describing the primary's selected item.
- `impact` — the narrow stack of readings *about* that subject (Recovery's
  per-task CPU, memory, disk and network).
- `rail` — the trailing `ActionRail` of commands for the selected item.
- `footer` — the section's status line and its section-wide controls.

Each section declares the regions it wants in *logical* lengths as a
`SectionAnatomy`; `resolve_section_frame` resolves them against the client
and, when the window is too narrow to seat them all, drops the optional ones
in one fixed order — `detail`, then `impact`, then `rail`, then `sidebar` —
so `primary` always survives and the drop order is a property of the frame
rather than a per-section improvisation.

**`primary` has a declared floor, and shedding honours it.** A region is shed
when `primary` would fall below `SectionAnatomy::primary_floor`, not merely
when it would reach zero. A section whose rows carry inline commands declares
how many (`primary_row_commands`) and the frame turns that count into the
width the strip needs — the commands, the gap keeping them off the row's
text, and the row's trailing inset. The count is declared rather than the
width because the width needs the theme and the live `Scale`;
`primary_floor` and the row splitter share one arithmetic
(`frame::action_button_width` / `frame::row_commands_width`), so a declared
floor cannot drift from the strip a row draws.

`panel.rs`'s `MIN_WIN_WIDTH`/`MIN_WIN_HEIGHT` stay the panel's *readability*
floor rather than becoming derived values: the floors need the theme's
control metrics and the live `Scale`, so they cannot produce a `const`. The
two are *tied* to the anatomies by a test asserting the minimum window keeps
every section's `primary` at its declared floor and keeps every sidebar and
rail any section asks for. `MIN_WIN_WIDTH` is **not** raised to the width at
which every optional column fits: shedding is the drop order working as
designed, and enlarging the window until shedding stops would be mitigation,
not a floor.

**Resources sheds its sidebar's route into the band, never its
destinations.** When the device rail is shed, the band grows a `ComboBox`
naming the current device, whose list is the same device set the rail held
(`09-theme-and-shed.png`). Losing the rail must not lose a pane, so the
control that replaces it is a control, not an omission.

An `ActionRail`'s column is one width wherever it appears
(`frame::ACTION_RAIL_WIDTH`), so a reader who learns where the commands sit
in one section finds them in the same place in the next.

Each section is a struct in its own module owning its view models, its
retained controls, its cursor and its section-private overlays, reached
through one `SectionView` dispatch (`anatomy`, `adopt`, `render`,
`on_pointer`, the content/action cursors, `activate_focused`, and the primary
column's scroll extent). `view/mod.rs` holds the window frame, the chrome,
the scroll model, the region focus policy and the one `match` that names the
active section — never a second copy of a section's behaviour.

## S4 — The three sections — planned

**Three sections, one per question a reader arrives with:** what is running,
what is this machine doing, what broke. `Section` and the wire
`CommandSection` both carry exactly `Tasks`, `Resources`, `Recovery`.

Every other surface the old design gave a section to is absorbed into the one
that was already about it:

| Absorbed | New home |
|---|---|
| Background (jobs) | no job registry exists anywhere in the system, so the section had no rows to show. Returns as a `Jobs` tab and a `Type` column on Tasks when a registry lands — not as a section. |
| Pressure | a banner on the Resources pane it names, carrying the same recommended relief and the same refusal kinds. A cause and its resource were never two places. |
| Activities | window grouping is the session's business, not the monitor's: it becomes the `Group by` control already on the Tasks table, and a group header row carries the four commands through the existing `primary_row_commands` machinery. |
| System | its four graphable pages *are* the Resources panes. Identity, Sessions and Permissions become a **Machine** group in the same device rail. Services and Power stated an absent interface and still do (S6). |

Nothing with a reading behind it is dropped. `PressureClock` and the
per-resource cause model survive as the banner's source; the fault model,
`FaultClock` and the crash-record match survive unchanged in Recovery.

**Selection must survive a refresh.** A master/detail section is unusable if
the detail pane changes object every time a sample lands, so every view model
carries the model's stable identity for its item and a section re-resolves
its selection against that identity after `adopt` (`view::resolve_selection`),
dropping it only when the item genuinely went away. A row number would
silently re-point at a different subject the moment one above it left. The
view never interprets an identity; it only compares.

### Tasks (`01-tasks.png`)

- **band summary** — four census `MetricTile`s (Processes, Users, Cores busy,
  Alerts), each plated and carrying the glyph of the thing it counts, tinted
  by a `PressureKind` used as an identity colour rather than as a claim that a
  resource is strained. `CENSUS` is their one declaration: the tiles are built
  from it and the room the band asks for is measured from it, so the band can
  never seat a different number of tiles than the section draws.
- **header** — the filter `Tabs` strip on its own row, whose labels carry each
  filter's count, then a `SearchField` matching on task name,
  case-insensitively, over its own full-width row. *Which kind* of task and
  *which* task are separate questions, so each gets a row. Every tile and
  every tab counts adopted rows through the *same* predicate, so a tile and
  its tab can never state different numbers.
- **primary** — a sortable `TableHeader` over `TableRow`s: Task (its
  `IconKind` and name), Owner, State, Activity (a per-task CPU `Chart`
  sparkline), CPU, Core, Memory, Disk, Network. Every column is a *reading*
  about the task; what may be done to it is the rail's business. Sorting is
  the header's, applied over the filtered rows and stable, so rows it cannot
  separate keep the order the sample reported. `COLUMN_WEIGHTS` is the one
  definition of the column geometry: the heading, the cells and the
  sparkline's own rect (`TableRow::cell_rects`) all read it.
- **rail** — `ACTIONS` for the *selected* task in a trailing `ActionRail`
  seated in a `Panel` that captions it, so the commands stay anchored while
  the rows scroll beneath. `RAIL_COMMANDS` declares them in reading order —
  Switch to, Reveal window, Pause, Resume, Lower priority, Open logs, Group…,
  Force quit. Force quit is `ControlRole::Destructive`, so it wears the danger
  rim and sits last, where a mis-aimed press is least likely to land. Every
  item renders its own verdict: permitted, plainly disabled where the task's
  state rules it out, or the Authority Mark where the caller lacks the
  authority. With nothing selected the rail holds no commands rather than a
  column of refusals, and the plate keeps its place either way.
- **footer** — the shown/total count and the Auto-refresh `Toggle` beneath the
  table, and the grouping `ComboBox` beneath the rail, so each control sits
  under what it governs. Auto-refresh holds the table on the sample the reader
  is reading rather than moving it under them.
- **cursor** — the content cursor spans header stops, then rows, then the
  rail's commands, then footer stops, so every control is keyboard-reachable
  without hanging off a row a filter could remove. `SectionView::focus_row`
  maps a cursor stop back to the row it names (`None` for the chrome bands and
  the anchored rail), keeping the scroll-into-view arithmetic in `view/mod.rs`
  as the one definition; `item_count`/`list_info` mean the filtered, sorted
  rows alone.

**The filters are the ones a reading backs.** All, Mine, System and Faults:
owner comes off `ProcessRecord::uid`, and Faults is the shared
`process_recovery` classifier that already resolves both stopped and
seat-reported-unresponsive tasks, so the tab, the rows' Signal Beads and the
Recovery section can never disagree about which tasks are faulted. `Jobs` and
`Services` are *not* spelled as tabs: with no job registry and no service
manager, every row is a process, and a tab that can only ever read `(0)` is
chrome. They return with their registries, alongside the `Type` column.

**Owner and Core are the columns this replaces them with, and both are real.**
`ProcessRecord` carries `uid`, `gid` and the CPU the task is dispatched on, so
a busy core in the CPU pane can be traced to the task sitting on it, and
per-principal accounting is visible on a machine with many users.

**The commands act on the selection, not on a row.** A `ProcId` — the task's
stable, never-reused instance identity — is what the selection remembers, so
it survives a refresh, a re-filter and a re-sort rather than following
whichever row slid into its place, and it drops only when the task genuinely
goes. A table with rows always has one selected, so the commands always have a
subject. This is what lets the rail state a task's whole repertoire instead of
the one or two buttons a row's trailing cell could hold.

`TaskAuthority` carries one verdict per command, reached in `model.rs` where
the caller's authority *and* the task's lifecycle state are both known:
signalling needs `PROC_CONTROL`, and with it the state still rules out what
makes no sense (pausing a stopped task, resuming a running one, anything at
all for a task that has already exited). `apply_action` re-checks that same
verdict before acting, so a command drawn as denied or disabled can never be
carried out by an unexpected report of it. `TaskControl::Reveal` is the same
request of the session as `Switch` — raising the window is how this system
shows a reader where it is. `TaskControl::OpenLogs` is permanently disabled:
no capability-gated query for a task's own log entries exists (S6), so the
command states its absence rather than pretending to work.

**A row wears no activity seam.** An activity in a control's state paints a
Heat Seam along its whole lower edge, which under a table row reads as an
orange rule beneath every working task rather than as a reading about one. A
task's activity is shown in the Activity column instead, as the sparkline the
heading promises; the row's state carries only its pressure (a Pressure Rail
in the leading gutter) and its recovery posture (a Signal Bead).

**Disk** is a real measurement: `TaskMeters` (`model.rs`) deltas each task's
`io_bytes_read + io_bytes_written` against its own previous reading over
`Sample::elapsed_ns`. A first sample, a task first seen this sample, and an
unmeasured interval each yield no rate (a cumulative total is not a rate); a
counter that did not move over a real interval is a genuine `0`. **Activity**
plots the same store's bounded per-task CPU ring (`TASK_HISTORY_LEN`, which is
`MAX_CHART_SAMPLES`), keyed by `ProcId` so a recycled pid cannot inherit a
dead task's history, and rebuilt from each sample so an exited task leaks
neither its history nor its counters. **Network** has no interface at all (S6)
and renders the explicit unmeasured mark.

### Resources (`02`–`07`)

The section that replaces the old System page list. Resources is **one pane
per resource device**, instrument-led: the old design rendered per-core load,
memory detail and compositor cost as `Vec<SystemFact>` key/value text, which
is the defect this section exists to fix. A resource's shape over time is the
reading; a fact list cannot carry it.

- **sidebar — the device rail.** One entry per *discovered device*, grouped:
  `Resources` (CPU, Memory), `Storage` (one per mounted volume), `Network`
  (one per managed interface), `Graphics` (the compositor), `Accelerators`
  (one per matching hardware-tree node, S8), then `Machine` (Identity &
  uptime, Sessions & seats, Permissions & limits). Each device entry carries
  its name, its current reading and its own bounded trace, so the rail is a
  live summary of the whole machine and the pane is the detail of one part of
  it. The `Machine` group's entries carry no trace: they are facts, not rates,
  and the absence of an instrument is what says so.

  The rail is the *sidebar* region, so it is the vertical `Tabs` control
  (S7) — a sidebar is not a second selection control. Cores are deliberately
  **not** rail entries: the CPU pane shows every core at once, so a per-core
  rail would state the same readings twice and push the devices off screen.

  **The rail's length is discovered, never declared.** Twelve cores, four
  volumes and three interfaces is the design case; a hundred-core machine with
  a dozen volumes gets a scrolling rail, not a truncated one, and no entry
  count is a compile-time constant.

- **header — the pane's hero.** The device's headline reading, its context
  line, and its instrument: a `Chart` trend where the reading is a rate (CPU,
  disk, network, graphics) and a `Track` where it is a fraction of a measured
  whole (memory, capacity). The choice belongs to the reading, not the
  renderer. A rate has no fixed ceiling to fill a bar against.

- **primary — the pane's own detail**, per device:

  - **CPU** (`02-cpu.png`) — the per-core grid: one unplated cell per logical
    CPU carrying the core's own trace, its busy percentage, its live measured
    clock and its performance class. Then the processor fact columns.
  - **Memory** (`03-memory.png`) — the composition bar (S7) answering *where
    did it go* in one row, then the memory and kernel fact columns, then the
    bounded-cache reclaim ledger.
  - **A volume** (`04-disk.png`) — the service-and-queue block, the capacity
    and medium block, and the health block: every completion bucketed, with
    the status pill the buckets resolve to.
  - **An interface** (`05-network.png`) — link and addresses, counters and
    offloads, and the stack block (sockets, resolver, time servers, defence).
  - **Graphics** (`06-graphics.png`) — the frame-work breakdown, the
    compositing path, and the graphics device.
  - **An accelerator** (`07-accelerator.png`) — what the node's discovery
    genuinely reports, and the readings awaiting S8's query. This pane lands
    with **D1**, not before: there is no `HwDeviceClass` for an accelerator
    yet, so discovery reports no such node, the rail grows no `Accelerators`
    group, and a pane written ahead of it would be code nothing can reach.
  - **Machine** — identity and uptime; the seats and logged-in census; the
    authority summary with the resource limits and their live usage.

- **A resource under pressure wears a banner on its own pane**, above the
  hero: the band, how long it has stood there, and the model's own recommended
  relief as a primary `Button`. Where this session cannot take that relief the
  banner names which refusal — `not permitted` for want of the capability, the
  plain disabled treatment otherwise — while the command still fails closed at
  its button, to the keyboard as to the pointer. A resource recommending
  nothing says so instead of volunteering another command.

  A band's age has no interface behind it (nothing timestamps a band change),
  so `PressureClock` tracks when each resource entered its band — clocked off
  the monotonic uptime reading, sharing one `elapsed_since` definition with
  `FaultClock` — and forgets it the sample the band eases, so a resource that
  comes back under pressure is timed from its new band. With no uptime reading
  the age reads unmeasured, never a fabricated zero.

- **Top consumers.** The CPU, Memory and volume panes each carry the five
  tasks costing that resource most, from the per-task readings the process
  record already provides, so the pane and the Tasks table can never disagree.
  **Summing them is not the device's total** and the pane says so: filesystem,
  RAID and swap traffic belongs to no process. The interface pane has no such
  block — per-task network has no interface (S6) — and states that in words
  rather than showing an empty list, because an empty list reads as *none*.

- **rail** — the commands for the *selected device*, seated in a `Panel`
  because `ActionRail` carries no caption of its own. Every action emits a
  typed view action the service authorises and applies; the view performs no
  privileged work. A refusal names its own kind: an action refused for want of
  a capability wears the Authority Mark, because acquiring the authority would
  make it available, while an action with no endpoint behind it is plainly
  disabled.

- **footer** — the sampling cadence and window, and the Auto-refresh `Toggle`.
  A pane that states its own averaging window is the difference between a rate
  a reader can act on and a number.

- **cursor** — the rail's device entries, then the pane's own stops, then the
  action rail's buttons, so Up/Down walks the device list as a reader expects
  of a vertical list and Enter/Space commits. The `Tabs` control's own
  vertical navigation is deliberately not fed the same keys, which would give
  them two meanings.

**How a pane is laid out, so every pane scrolls the same way.** A pane
compiles to a flat run of short, self-contained drawables, each knowing its
row, its row span and its column *before* any paint, so a paint allocates
nothing and lays nothing out — it walks the items the viewport covers. Spans
are fixed and width-independent, which is what makes the scroll range exact
and lets a pane taller than its viewport scroll a row at a time. Two
consequences:

- **The blocks flow in one or two columns**, a `Half` block pairing with the
  next one and the pair advancing by the taller side. A `Full` block closes an
  unpaired half first, so a column can never overhang the block below it.
- **The per-core grid's cells-per-row is a *layout input* to the compile, not
  a constant**, because the grid re-wraps rather than squeezing: a pane too
  narrow for six cells draws fewer per row and scrolls. A width change
  therefore has to *recompile* the flow, and `SectionView::render` and
  `list_info` are both `&self` — so this needs one `&mut` relayout hook on
  `SectionView`, called from `Switchboard::render` before `sync_scroll`.
  Recompiling per paint instead is the §28 defect (work scaling with the
  surface rather than with the change).

**The memory composition's parts are the ones the kernel accounts.** The board
sketches a Linux-shaped anonymous / file-cache / slab split; no reading behind
it exists. The honest segments are what user address spaces hold, what the
kernel's own heaps hold, what the reclaimable classes hold, what the
compressed tier holds, whatever those named parts do not account for, and the
free remainder — which closes the whole exactly, so the bar cannot
under-report where the memory went.

**A device command is labelled, not glyphed, and almost none has an
endpoint.** The vocabulary these rails need — scrub, trim, renew a lease, drop
a cache, unmount — has no shipped `IconKind`, and an icon with no built-in
glyph behind it is not one this desktop may draw (§10), so the rail is
labelled like the machine-actions rail already is. Of the commands the boards
name, only "sort tasks by *resource*" is a command this service can carry out:
it is a view transition, the same shape as the pressure card's "Show tasks".
Every other one is plainly disabled for want of an endpoint — never marked for
authority, because acquiring a capability would not make an absent endpoint
appear.

**Two rail entries carry no trace, and that is a reading about them.**
A volume's capacity is a level rather than a rate, and there is no per-volume
byte counter to delta; the interface rates query serves an already-averaged
reading rather than a counter, so a trace would plot someone else's averaging
window. Both entries therefore show their reading without an instrument until
Q1 lands.

**The Graphics pane is named for the display path, not for a GPU.** A
framebuffer-only or headless machine has no GPU and would read an empty *GPU*
pane — but it still composites, and that work is what a reader needs. So the
pane leads with the compositor's measured frame cost and treats the device as
one of its facts. The line that earns the block is **damaged px against
blended px against screen px** — "we blended 4.2 M pixels to change 3 200" —
with `Opaque copies`, `Rectangles`, `Present calls` and `Window furniture`
behind it. An idle frame reads *idle*, not a row of zeros pretending to be a
frame; a frame nobody has reported yet reads unavailable. **Counts of work
only:** no wall-clock figure rides this path, because a duration is neither
reproducible nor assertable.

That reading is the one this service does not sample. The session owns the
compositor, so it reports what its last frame cost
(`SwitchboardCommand::FrameReport`) over the command port it already sends the
seat report on, on that same discipline — only with a live consumer, only when
the counts changed, never blocking a frame path, a dropped stale report being
fine because the next frame re-sends a fresher one — **and never when the only
served content that landed was this service's own window**. A monitor must not
measure its own act of displaying: reporting a frame whose work was only the
panel painting the previous report re-excites another paint forever. The
session classifies presents by attested owner and suppresses a
Switchboard-only frame; real desktop work and chrome/idle settles still report.
The receiver validates it and refuses counts no compositor pass could have
produced, so the panel never renders a sender's arithmetic.

### Recovery (`08-recovery.png`) — done

Kept as its own section: triage is a different job from reading a resource,
and folding it into a table filter would lose the timeline, the crash snapshot
and the impact stack that make triage possible.

- **primary** — a `Card` per fault: name, what happened, and how long ago.
- **detail** — the fault's identity, a `StatusPill` naming its impact, a
  `FactList` (status, age, recommendation), then a `Tabs` strip over three
  pages: Timeline (the marks this service observed), Crash Snapshot (the
  kernel's `CRASH_RECORD` — fault class, distance from its anchor, access
  direction, owning uid/gid, `pc`/`sp`/`fp`, every named register and every
  backtrace frame), and Logs (no interface, stated).
- **impact** — a stack of unplated `MetricTile`s for the faulting task's CPU,
  memory, disk and network; network is always unmeasured (S6).
- **rail** — `RECOVERY ACTIONS`, carrying only the commands this service
  backs: Restart, Soft quit, Save crash record, and Force with its
  confirmation posture or the Authority Mark.
- **footer** — the resolved-fault count, carried in the model because only
  something folding one sample into the next can see a fault clear.

A section whose master list is `Card`s makes its selection through one shared
walk (`view::select_pressed_card`): it offers the pointer event to the card in
each visible slot and reports whichever answered along with its own
`CardAction`. A body press selects the card, so pressing a card opens its
detail; a footer click selects it *and* resolves that button's command, so a
command can never act on a subject other than the card that offered it.

The crash record is matched to its fault by `ProcId` and nothing else: a
numeric pid is reused, so matching on one could attribute a dead task's crash
to a live task that inherited its number. A fault with no record says so
plainly and does *not* wear the unmeasured mark — a stopped or unresponsive
task has faulted without ever raising a user fault.

A fault's age has no interface behind it either, so the service tracks when it
first saw each task faulted, keyed by `ProcId`, clocked off the monotonic
uptime reading, pruned the first sample the fault clears, and counting each
pruned entry as the resolved tally.

## S5 — What the service samples — planned

The staged cadence (`Cadence::EverySample` / `Memory` / `Inventory` /
`Static`) is the existing design and stays: a reading is issued on the tier
its subject actually moves at. Each reading is its own sampled field,
capability-gated at the query, degrading exactly the field it backs and
nothing else. A field the sampler could not read carries an `Absence` saying
*which* — a scope the caller does not hold reads `not permitted`, a query that
simply did not answer reads `unavailable` — because those are different
statements to a reader.

The sample set gains S8's queries, all on `EverySample` because each is a rate
source whose delta is the reading:

| Query | Tier | What it backs |
|---|---|---|
| `VOLUME_IO_STATS` | `EverySample` | a volume's throughput, IOPS, utilisation and await |
| `VOLUME_IO_QUEUE` | `EverySample` | a volume's in-flight count and mean queue depth |
| `GPU_DEVICE_STATS` | `EverySample` | graphics engine busy share, device memory, `AccelCaps` |
| `ACCEL_DEVICE_STATS` | `EverySample` | an accelerator's busy share, memory and queue |

**The panes need ten readings the sampler does not take yet, all of them
already served by an existing query.** S8's four are the ones that need *new*
queries; these need only sampling, and `lib/procinfo` already has a helper for
each, so the work is a `DegradedField`, a `Sample` field, a cadence entry and a
scope entry apiece:

| Reading | Tier | Scope | What it backs |
|---|---|---|---|
| `MEMORY_PRESSURE_BAND` | `EverySample` | ungated | the band and the banner, on a ceiling without kernel readings |
| `RECLAIM_STATS` | `Memory` | kernel | the composition's reclaimable share |
| `RAMZIP_STATS` | `Memory` | kernel | the compressed tier |
| `CACHE_LEDGERS` | `Memory` | kernel | the bounded-cache reclaim ledger |
| `NET_INTERFACE_COUNTERS` | `EverySample` | global | the interface counters block |
| `NET_SOCKETS` | `EverySample` | global | the socket census (folded to two counts as it walks) |
| `NET_RESOLVER_SERVERS` | `Inventory` | ungated | the stack block's resolvers |
| `NET_TIME_SERVERS` | `Inventory` | ungated | the stack block's time servers |
| `NET_STACK_DEFENCE` | `EverySample` | global | the SYN-backlog defence reading |
| `HARDWARE_TREE` | `Inventory` | hardware | the graphics device's identity |

`CACHE_REPORT` is **not** among them: it is how a process *submits* its own
cache rows, not a reading, so the ledger is `CACHE_LEDGERS` alone.

**Per-core busy is a `CPU_TIME_STATS` walk, not a new query.** The aggregate
the sampler already derives is a sum over per-CPU records that each carry
their own `busy_ns`/`idle_ns`, so walking the records instead of the existing
aggregate helper yields both readings from one read. A core first seen this
sample contributes no share — a cumulative total is not a share.

**The rail's traces and the per-core cells need a rolling store the sample
does not carry**, the per-device counterpart of `TaskMeters`: each core's own
bounded busy history, and each device's previous cumulative counters with the
rates they produce. Keyed on the subject's own identity (a CPU index, a volume
id, an interface name) rather than a rail position, and rebuilt from the
sample so an unmounted volume leaks neither history nor counters. A byte rate
needs a shared full-scale reference to be plotted in permille at all; one
reference across every device is what makes two rail traces comparable by eye.

**`CPU_INFO` moves from `Static` to `EverySample`.** Its `current_freq_hz` is
a live reading and `CPU_INFO_FLAG_FREQ_MEASURED` exists precisely so a
consumer can trust or discard it; leaving the query on the static tier is what
made the old surface unable to show a live clock. The record is 88 bytes per
core, so a 128-core machine costs ~11 KB per sample — well inside the
transport, and the price of a live reading. The immutable part (model, class,
feature bits, reference clock) is re-read with it because the query is not
field-selective; if that ever matters, the fix is a request flag asking for
the live fields only, and `CpuInfoListRequest::flags` is already reserved for
one. It is not added before there is a measurement saying it is needed.

Per-task CPU history for the Activity sparkline is the service's own: a
bounded per-task ring of the CPU permille it already measures, keyed by the
task's stable identity, so a sparkline plots measurements rather than a shape.

## S6 — Readings with no interface yet

These render an honest unmeasured mark (`MeterValue::Unmeasured`, an
unmeasured cell, an empty `Chart`'s quiet plate, or a page stating its
reason), never a fabricated number. An empty list is *not* such a mark: it
reads as "none", so a reading with no interface states its absence in words.
Each needs its own interface before the surface can fill in; the layout
already has the slot.

| Reading | What is missing | Owner |
|---|---|---|
| per-task network bytes | no per-process socket accounting | the userland network service, which owns the sockets (`plans/NETWORK.md`) |
| per-task uptime / last-active | `ProcessRecord` carries no creation timestamp | the kernel process record |
| service list with state/CPU/memory | no service manager exists | `plans/NEW-SERVICEMANAGER.md` |
| background jobs with progress | no job registry exists anywhere | unowned |
| temperature, AC/battery | no sensor or power-supply interface, and no driver to serve one | a `drivers/sensor/` class |
| log reading | the journal has an ingress path but no capability-gated read query | `plans/SYSLOG.md` |

Per-task **disk** bytes are measured: the kernel accounts the bytes each
process's own file reads and writes transfer, reported on `ProcessRecord`, and
the view derives a rate from the delta between samples.

## S7 — Controls this surface needs

Generic, reusable, and complete on landing (every state, both themes, the
heavier-contrast path, pointer and keyboard where interactive).

**Already in `lib/controls`, contributed by this surface — done.**
`nav::Breadcrumb` (a location trail whose trailing crumb is the current
location and is not activatable, eliding oldest-first with an activatable
ellipsis so the current location is never dropped); `metric::MetricTile` (an
optional identity icon, a label, a large reading with a quiet unit, an
optional detail line, and an optional `MetricInstrument` — a proportional
track reusing `MeterValue`, or a `Chart` trend — whose `MetricLayout` chooses
the stacked or inline form, and which draws no plate when unplated so a stack
of readings shares one container's surface); `metric::StatusPill`;
`record::FactList` (right-aligned key/value readouts where the value keeps its
room and the label truncates first); `record::Timeline` (a spine spanning only
first to last mark, shape-coded marks, a stamp column sized to the widest
stamp); `rail::ActionRail` (the vertical counterpart of `Toolbar`, composing
`Button`s so plate, role, disabled and denied rendering are not restated);
`collection::TableHeader` (sortable column titles sharing the row family's one
column-width model, reporting a sort the owner commits); and
`tabs::TabsOrientation` (a vertical orientation of the existing strip, so a
sidebar is not a second selection control).

**The three additions this plan needed — done.** Everything else the surface
composes from the above. Each is specified with its settle-point and damage
obligations in `plans/GUI-CONTROLS-DESIGN.md` (§11.35, §11.40, §11.12).

- **`chart::Chart` has an opposing series.** A read/write or receive/send rate
  is one reading with two directions, and drawing it as two stacked charts
  loses the comparison that matters. `with_opposing` takes a second series,
  plotted mirrored below a drawn axis and tinted by its own `PressureKind`.
  One chart control and one plot path, not a second `DuplexChart` beside the
  first — the bounded `MAX_CHART_SAMPLES` window, the empty-series groove and
  the area treatment are reused whole. Adding a series *asserts the direction
  is measured*: a direction with no reading behind it is left off, so the
  chart stays a single-series trend over the whole box; an axis is drawn only
  where something is plotted, and a box too short for both halves degrades to
  the quiet plate rather than half a reading.
- **`metric::CompositionBar`** — named proportional parts of a measured whole,
  with a key naming each part and its amount. Answers *where did it go* for
  memory composition and for capacity by class. Shares that do not sum to the
  whole are a `CompositionError` at construction, not a silently short bar.
  The parts separate by *hue* — a fixed rotation of the theme's own resource
  colours, led by the bar's own resource — because they are categories rather
  than degrees, and the joins are ruled so they stay countable on the
  monochrome-safe path. The part that is *not* in use is declared as the
  composition's `remainder`: the track's quiet neutral, last, still named in
  the key. It draws through the one measured-track geometry (`TrackBand`) in
  `controls::paint`, which `MetricTile`'s `Track` now shares. The key wraps
  rather than dropping a part, so `measured_height` takes the width it will be
  given.
- **`tabs::Tabs` (vertical) is a sidebar list.** The device rail is a sidebar,
  and a sidebar is not a second selection control. An entry's label leads with
  its live reading trailing on the same line, an optional bounded `Chart`
  trend draws beneath, and a quiet group heading may introduce the entry that
  *starts* a group — declared by that entry, so a heading can never point at
  one that is not there. Selection, focus and keyboard behaviour are reused
  whole. Entries **stack** at their own content height rather than sharing the
  column, which is what makes V1's discovered rail scroll rather than squeeze:
  `Tabs::measured_height` states the height the whole list wants. Because a
  vertical entry's rectangle depends on the theme's metrics, the hit test and
  every damage-reporting entry point take the scale and theme the strip was
  laid out with — the shape `ActionRail` already had; `Tabs::tab_area` is the
  forward mirror of `tab_at`, so a caller (or a pointer-driven test) aims at
  the rectangle `render` painted.

The measured-track geometry — groove, proportional tinted fill, pressure
outline — keeps its one definition in `controls::paint`, which `MetricTile`'s
`Track` instrument and `CompositionBar` both draw through; it is the only
reading-with-a-track in the design language.

Controls this surface deliberately does **not** add, because they are
composition rather than behaviour: the pressure banner (`Panel` +
`StatusPill` + `Button`), the per-core cell (an unplated `MetricTile` with a
`Chart` instrument and a `StatusPill` badge), the top-consumers row
(`TableRow` with a track cell), and the per-core grid itself, which is the
pane's layout and belongs to the pane.

## S8 — The interfaces the resource panes need — planned

Four queries, each modelled on an existing one so its gate and its shape are
not a new argument. Every one is paged by an `offset`/`limit` request, so a
fixed transport buffer never bounds how many devices a machine may have, and
every count, byte and duration is 64-bit.

**`VOLUME_IO_STATS` — ungated.** The storage analogue of `CPU_TIME_STATS`,
and ungated for the same reason: a machine-wide utilisation figure is one
every user may see, and it exposes strictly less than the already-ungated
`MOUNT_LIST`. Per volume, keyed by the same 16-byte `volume_id`:

- `read_bytes`, `write_bytes` — cumulative bytes transferred since attach.
- `read_ops`, `write_ops` — cumulative completed requests.
- `busy_ns` — cumulative time the device had at least one request in flight.
- `read_wait_ns`, `write_wait_ns` — cumulative time requests spent between
  issue and completion, summed per request.

Throughput, IOPS, utilisation and await are all two-sample deltas of these:
utilisation is `busy_ns` delta over the sample interval, await is `wait_ns`
delta over `ops` delta. Nothing is served pre-derived, so no consumer inherits
another's averaging window. A first sample yields no rate, exactly as the
per-task disk rate does.

**`VOLUME_IO_QUEUE` — `CAP_SYSINFO_KERNEL`, audited.** The exact analogue of
`CPU_LOAD`, gated for the same reason: a queue depth is a driver and scheduler
internal, not the utilisation split every user may see. Per volume:

- `in_flight` — requests outstanding at the sample instant.
- `queue_depth_sum`, `queue_samples` — so a *mean* depth is a delta ratio
  rather than one instant's snapshot, which is what a reader actually wants.
- `budget_depth`, `budget_deadline_ns` — the `BlkDeviceClass` budget in force,
  so a depth is read against the ceiling that applies to that medium.

**`GPU_DEVICE_STATS` — `CAP_SYSINFO_HW`.** Gated with `HARDWARE_TREE`, whose
device inventory it details. One record per graphics device, plus one per
engine so a machine that reports engines separately is not flattened:

- `busy_ns`, `idle_ns` per engine, with an engine class (render, blit, video
  decode, video encode) — the same busy/idle vocabulary as the CPU, so
  utilisation derives the same way and no new averaging convention appears.
- `mem_resident_bytes`, `mem_total_bytes` — device memory, `0` total meaning
  the device has no memory of its own rather than none free.
- the device's `AccelCaps` (`max_layers`, `max_width_px`, `max_height_px`,
  `per_layer_opacity`), which exists in the display driver ABI today with no
  query publishing it. Publishing it here is what lets the Graphics pane's
  compositing-path facts stop being unmeasured.

**`ACCEL_DEVICE_STATS` — `CAP_SYSINFO_HW`.** The same shape for a
general-purpose accelerator: `busy_ns`/`idle_ns`, `mem_resident_bytes`/
`mem_total_bytes`, `in_flight`. One record per device, paged.

Each query enters `SYSINFO_QUERIES` with its `SysinfoQuerySpec`, and
`sysinfo-v1` is not frozen, so these are added in place with every consumer
updated in the same change. None of them adds a capability: the existing
`CAP_SYSINFO_KERNEL` and `CAP_SYSINFO_HW` already express exactly the two
boundaries involved, and a capability with no boundary of its own is not
added.

**An accelerator also needs a driver class before its pane has a device to
describe.** `drivers/accelerator/` with its trait in
`lib/abi/src/driver/accelerator.rs`, so a hardware-tree node binds through the
ordinary discovery-match path and never by naming a part. Until then the pane
reports what discovery genuinely knows — the node, its class, its match keys,
and that no driver matched — which is a real state, not an error and never a
panic.

## S9 — Palette and pressure-kind additions — planned

`PressureKind` stops at Cpu, Memory, Disk, Network, Power, Thermal, so a
graphics or accelerator reading has no identity colour and its `Chart` cannot
be tinted. Two variants and two palette entries, in both built-in themes:

| Variant | Palette entry | Dark | Light |
|---|---|---|---|
| `PressureKind::Gpu` | `gpu_pressure` | `#22b8a6` | `#0f8478` |
| `PressureKind::Accelerator` | `accelerator_pressure` | `#d94f8c` | `#a82f66` |

Teal sits clear of every existing hue. The accelerator magenta is 57° from the
memory violet and 28° from the recovery red — close to the latter in hue, but
`recovery` is only ever a fault signal and never tints a resource chart, so the
two cannot appear in the same role on one surface. Both are added with the
panes that use them, not ahead of them, and both are checked on the
heavier-contrast path and on paper-white.

## S10 — What this change deletes — planned

Superseded code is deleted, not renamed or left dead.

- `view/background.rs`, `view/pressure.rs`, `view/activities.rs` and their
  test modules — the sections go. `PressureClock` and the per-resource cause
  model move to the Resources banner; the group model moves to Tasks'
  grouping. The `Card`-based job list and the job action rail have no registry
  behind them and go entirely.
- `view/system.rs` and `view/system_data.rs` and their tests — replaced by
  `view/resources/`. The `PageLine` vocabulary (heading / fact / absence) and
  `SystemReport`'s `cores`, `memory` and `compositor` `Vec<SystemFact>` fields
  go with them: rendering a resource as key/value text is the defect this
  plan fixes. The reports that are genuinely fact lists — machine identity,
  seats, limits — survive as the `Machine` group's panes.
- `Section::{Jobs, Pressure, Activities, System}` and
  `CommandSection::{Jobs, Pressure, Activities, System}` — replaced by
  `Resources`. `map_section` and its exhaustive test table shrink with them.
  The wire discriminants are renumbered rather than left with holes:
  `sysinfo-v1` and the Switchboard IPC are unfrozen, and a reserved gap that
  nothing will ever fill is the compatibility debt the charter forbids.
- Any `TileInstrument`/`HeadlineTile` machinery that only served the old
  four-tile System header, once the pane heroes carry their own instruments.

**The shared reading vocabulary needs a surviving home before
`system_data.rs` goes.** `Reading`, `Unmeasured`, `absence_statement`,
`reading_text`, `selection_prompt`, `HealthSeverity` and the labelled-reading
pair are read by Recovery and Tasks as well, so they move to their own module
first; only the System-specific types (`SystemPage`, `SystemReport`,
`HeadlineTile`, `TileInstrument`, `PageLine`, `SystemAction`) are deleted.
`SystemFact` is renamed with the move — a type named after a deleted section
misleads every later reader.

`docs/src/desktop/switchboard.md` is rewritten in the same change: it
describes the section set, so it cannot survive the section set changing.

`view/tasks.rs`'s three rustdoc references to `plans/switchboard1.png` (the
column order, the census tiles, and the rail commands) re-point at
`plans/switchboard/01-tasks.png`, which is what now fixes those declarations.
The older `plans/switchboard[1-4].png` boards stay until then: they are still
the live reference those declarations cite, and are superseded only when the
code that cites them is rewritten.

## S11 — Consequences in other plans — planned

- **`plans/NEW-TASKBAR.md`** — the tray capsule and the long-press route open
  a `CommandSection`, so both are re-pointed: a flagged icon still opens
  `Recovery`, and T13's system quick-actions menu opens `Resources` on the
  `Machine` group, whose action rail carries the session and power commands
  that menu offers. T10–T12's sampling, summary and lifecycle contract is
  unchanged. S2's "no permanent resource band above the sections" still holds
  and is still what T12's per-column-one-instrument rule depends on.
- **`plans/NEW-DESKTOP-SETTINGS.md`** — unaffected: it shares the controls,
  not the sections. The vertical `Tabs` extension (S7) is available to its
  pane list.
- **`plans/FIX-DESKTOP-SPEEDUP.md`** — Stage A's frame counters surface on the
  Graphics pane rather than a System page. The counters, their validation and
  the self-report suppression rule are unchanged.
- **`plans/GUI-CONTROLS-DESIGN.md`** — gains the three S7 controls in its
  control families, with the settle-point and damage obligations every
  interactive control carries.
- **`plans/FIX-IO.md`** — `VOLUME_IO_STATS`/`VOLUME_IO_QUEUE` read the
  per-device health and budget machinery that plan already defines; the
  counters are folded there, not invented here.

## S12 — Responsiveness obligations — planned

The Switchboard is an interactive surface, so the desktop responsiveness rules
bind it directly, and a monitor is the easiest surface in the system to get
this wrong on.

- **Selecting a device performs no I/O.** The rail's selection changes which
  pane is drawn from state the sampler has already delivered. It issues no
  query, opens no store and waits on nothing; a pane with no sample yet reads
  unavailable rather than blocking for one.
- **The sampler is the worker.** It parks between cadence ticks and is woken
  by its timer, never spinning, and its results arrive as state the view
  adopts. A paint reads nothing.
- **A burst of input produces one paint.** Pointer motion over the rail, wheel
  deltas over a pane and resize samples all arrive faster than the screen can
  show them: the loop drains, then paints once.
- **A repaint is scoped to what changed.** A fresh sample damages the
  instruments whose readings moved and the rows whose cells moved — not the
  whole client. Re-deriving the pane because *a* sample landed is the defect,
  and it is worst exactly where the machine is slowest and the rail longest.
- **Auto-refresh holds the sample the reader is reading**, and toggling it
  changes only that: it does not re-query, reset a history or resize anything.
- **The frame report never measures this window.** The suppression rule in S4
  is a responsiveness obligation as much as an honesty one: without it the
  Graphics pane re-excites its own repaint forever.
