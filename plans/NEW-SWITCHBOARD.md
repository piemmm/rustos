# NEW-SWITCHBOARD — the Switchboard surface

Binding under `AGENTS.md`. This plan fixes the Switchboard window's
information architecture, the controls it is built from, and which of its
readings are real measurements today. The monitor *service* behind the
window — its sampling cadence, tray-summary contract, capability sizing and
lifecycle — is `plans/NEW-TASKBAR.md` T10–T12 and is unchanged by this plan.

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
- a section-list `IconButton` (`IconKind::ListMenu`) at the trailing end,
  which opens that same `Menu` of the six sections. The section on show is
  marked with `Menu::set_current` *and* the item's `SelectionState::Selected`
  — the pair `ComboBox` already marks its own choice with, not a second
  convention.

There is no global tab strip: the band is the whole section switcher, and
both routes run the one `select_section_index` transition, so the trail, the
content and the per-section scroll offset cannot disagree. The band is a
Tab-cycle focus region, so a section is reachable without a pointer: Space or
Enter opens the list, Up/Down walk it, Enter shows the section under the
cursor, Escape closes it unchanged. The list is modal while open, which is
also what keeps it and the Tasks Group popup mutually exclusive.

Directly under the title bar, *above* the location band, is the header
resource band: one read-only `Meter` column per `ResourceSummary`, each over
one instrument — a `Chart` where there is a history to plot, the meter's own
track where there is not. It takes no pointer or keyboard input and emits no
action, and an empty resource list collapses it to zero height rather than
drawing an empty strip. Moving those readings into per-section instruments
(the `MetricTile`s S3 wants at the top of Tasks and System) is part of the
section interiors, so the band stays until S3 lands.

Content taller than the viewport is governed by the one shared vertical
`ScrollBar`, with the `ResizeGrabber` at its junction. A row list's anchored
column of inline row actions carries `ActionRail`'s Edge Wake while that list
is scrolled away from its start, so a reader can tell the column is pinned.

## S3 — The six sections' interiors — planned

The six sections exist, each in its own module of `src/view/` with its own
view models, layout, painting and input, and each draws its data live. What
remains is the **interior** the reference designs show: today every section
is a plain vertical list of rows or cards under the shared skeleton, with no
header instruments, no filtering, no sortable table and no detail pane. This
is the next piece of work, done one section at a time.

What each still needs, against what it draws now:

- **Tasks** — draws `ListRow`s with a per-row action and a `Group` action.
  Needs the KPI `MetricTile` row, a filter `Tabs` strip (filtering tasks, not
  switching sections), a `SearchField`, a sortable `TableHeader` over
  `TableRow`s with a per-row `Chart` sparkline, and the footer
  `Toggle` + `ComboBox`.
- **Background** — draws a job `Card` list, always empty because no job
  registry exists (S5). Needs the detail `Chart` + `FactList` + `Timeline`
  beside the list, and a source.
- **Pressure** — draws one cause `Card` per flagged resource with per-cause
  verdicts. Needs the `FactList` detail inside each card.
- **Activities** — draws grouping rows with inline rename and set actions.
  Closest to its design; needs the design's header and member styling.
- **Recovery** — draws fault rows with restart/force actions. Needs the
  `Card` list with a detail pane (`StatusPill` + `FactList` + `Tabs` +
  `Timeline`) and the impact `MetricTile` stack.
- **System** — draws resource cards over a service `Panel`. Needs the
  vertical `Tabs` sidebar, the four `MetricTile`s, and one page per sidebar
  entry.

The row lists' inline action buttons are laid out beside each row by the
shared `split_row` geometry and stay each row's own retained controls. They
are not an `ActionRail`: a rail stacks its items contiguously from the top of
its bounds and owns them, which cannot express a scrolled window of a longer
list, nor the Activities list where button-bearing header rows interleave
with button-less member rows. Where the designs show a command column it is
the commands for the *selected* item, which is a genuine `ActionRail` and
lands with the section interiors above — not a re-housing of the per-row
buttons.

Two of the six are spelled differently in the code than here: `Section::Jobs`
titled "Jobs" is this plan's **Background**, and `Section::Overview` titled
"Overview" is its **System**. The section modules already carry the plan's
names (`background.rs`, `system.rs`); renaming the variants and their titles
to match belongs with each section's own interior work.

Every action emits a typed view action the service authorises and applies;
the view performs no privileged work, and a verdict that is absent renders
the Authority Mark rather than an enabled control.

## S4 — Real readings — done

The window reads the System Information API for: `SYSTEM_IDENTITY`
(hostname, machine id), `UPTIME`, `LOAD_AVERAGE`, `CPU_INFO`, `CPU_LOAD`,
`CPU_TIME_STATS`, `MEMORY_TOTAL`, `KERNEL_MEMORY_STATS`, `MEMORY_PRESSURE`,
`MOUNT_LIST`, `VOLUME_IO_HEALTH`, `NET_INTERFACE_FACTS`,
`NET_INTERFACE_STATE`, `NET_INTERFACE_RATES`, `SEAT_LIST`,
`RESOURCE_LIMITS`, `CRASH_RECORD` and the process list. Each degrades
exactly the field it backs.

## S5 — Readings with no interface yet — planned

These render an honest unmeasured mark (`MeterValue::Unmeasured`, an
unmeasured cell, or an empty section with a stated reason), never a
fabricated number. Each needs its own interface before the surface can fill
in; the layout already has the slot.

| Reading | What is missing |
|---|---|
| per-task network bytes | no per-process socket accounting; attribution would belong to the userland network service |
| per-task uptime | `ProcessRecord` carries no creation timestamp |
| filesystem capacity | no used/total report from the filesystem drivers |
| service list with state/CPU/memory | no service manager exists (`plans/NEW-SERVICEMANAGER.md`) |
| background jobs with progress | no job registry exists anywhere in the system |
| temperature, AC/battery | no sensor or power-supply interface, and no driver to serve one |
| log reading | the journal has an ingress path but no capability-gated read query (`plans/SYSLOG.md`) |

Per-task **disk** bytes are measured: the kernel accounts the bytes each
process's own file reads and writes actually transfer, reported on
`ProcessRecord`, and the view derives a rate from the delta between samples.

## S6 — Controls this surface added to `lib/controls` — done

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

The measured-track geometry is one shared helper in `controls::paint`, used
by both `Meter` and `MetricTile`.
