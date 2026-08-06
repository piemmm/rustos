# tairix-switchboard

The TAIRiX **Switchboard monitor service** (`plans/NEW-TASKBAR.md`
T10–T12): the dedicated, capability-sized process behind the taskbar's
always-right-most Switchboard icon. It samples the live system through the
System Information API, publishes a compact `TraySummary` to the desktop
session over the seat-scoped `SWITCHBOARD_ENDPOINT`
(`lib/abi/src/switchboard_ipc.rs`) which the session binds and the taskbar
renders as the tray signals, and hosts the live overview window that icon
opens.

It is deliberately **not** part of the desktop session's own binary: the
tray overview wants system-wide authority (`CAP_SYSINFO_GLOBAL`,
`CAP_SYSINFO_KERNEL`) that the session's manifest should never have to
carry. The session spawns `switchboard.app` as the logged-in user and reads
its summaries over IPC; the authority lives and dies with this one small
process (`AGENTS.md` §5.2 — capabilities are sized to the holder that
enforces them).

## What it samples

Each cycle gathers one `Sample` (`src/sample.rs`):

- **The process list** — system-wide when `CAP_SYSINFO_GLOBAL` was granted,
  the caller's own processes otherwise. From it: the count of `Stopped`
  processes (the tray's `recovery` signal), the **top task** — the
  process with the highest CPU-time delta since the previous sample, keyed
  on the stable, never-reused `proc_id` so numeric-pid reuse can never
  stitch two lifetimes together — and per process the kernel-attested
  owner uid, mapped bytes, and current scheduling service level, which the
  Pressure cards' verdicts and culprit attribution are built from. The
  first sample honestly has no top task: there is no interval to measure
  over.
- **Aggregate CPU time** — the shared `tairix_procinfo::CpuTotals` delta,
  yielding the overall busy fraction in permille.
- **Memory pressure** — the audited `MEMORY_PRESSURE` query (needs
  `CAP_SYSINFO_KERNEL`), on its own slower cadence (below). The published
  level is the honest used-memory fraction,
  `(total - free) * 1000 / total`, and the pressured/normal verdict is the
  kernel's own band (band ≥ 1), whose enter/exit watermarks already carry
  hysteresis.

`derive_summary` (`src/derive.rs`) turns a `Sample` into the wire
`TraySummary`. CPU pressure enters at ≥ 900‰ busy and exits below 800‰ —
the gap is hysteresis so a load hovering at the threshold cannot flap the
tray rail. When both CPU and memory are pressured, the higher level is the
dominant one shown (a tie favours CPU) and the pressure carries the count
of pressured resources. `jobs` is always `0` today: no background-job
registry exists in the OS, and the field stays an honest zero rather than a
fabricated count.

**Honest-data rules.** Every field is a real measurement or an explicit
absence. A denied or failed query degrades exactly the field it backs
(noted once on `stderr`, never spammed per sample); nothing synthesises a
plausible-looking value, and a top-task name that fails wire validation
yields no top task rather than a mangled one.

## The live overview window

The session's `OpenPanel` command shows this crate's own `Switchboard`
screen composition (`src/view/`: `mod.rs` holds the retained widget tree,
the window chrome, input dispatch, the scroll model and the shared
per-section layout skeleton, and one sibling module per section owns that
section's view models, layout, painting and input) on a requested section,
selected through its own `Switchboard::select_section`. `src/panel.rs` owns
the lifecycle and `src/model.rs` builds what it shows. The screen is
assembled purely from the shared `lib/controls` controls and paints no chrome
of its own; it lives here because it arranges those controls into one
particular window, while `lib/controls` holds only behaviour any surface may
reuse (`plans/NEW-SWITCHBOARD.md` S1).

Its chrome is the standard window frame and title bar plus a **location
band**: a `Breadcrumb` reading `Switchboard › <section>` with a trailing
`IconButton` that opens a `Menu` of the six sections, the one on show marked
selected and current exactly as a `ComboBox` marks its own choice. The
trail's leading crumb opens the same list — its trailing crumb is the current
location, which a breadcrumb never activates — and with the band focused,
Space or Enter opens the list, Up/Down walk it, Enter shows the section under
the cursor and Escape closes it unchanged. Both routes run the one section
transition, so the trail, the content and the per-section scroll offset can
never disagree.

There is at most **one** window. A second `OpenPanel` asks the session to
raise the one already open — naming this service's own pid, since the
session alone owns the window stack — and switches to the requested
section rather than stacking a second window. The window's close control
destroys it and the service returns to headless sampling; **sampling and
publishing continue unchanged whether or not a window is open**, because
the window is a view onto a monitor that never stops monitoring. The
system is re-sampled strictly on its 2 s deadline: an input or command
wake never re-queries the system. The model is rebuilt on that same
tickless cadence, and the panel presents **at most once per wake, and only
when what it would draw differs from what it last drew** — the
composition itself, the window's bounds, the active theme, and the render
scale (`src/panel.rs::Panel::flush`). A wake that delivered an event but
left every one of those unchanged, such as a pointer move that crosses no
control, costs no render and no present.

| Section | Source |
|---|---|
| Tasks | the sampled process list; the row action raises that owner's window, and its `Group` button files the task into an activity |
| Pressure | one cause card per resource the tray's own latches flag, naming the measured culprit (busiest task for CPU, largest mapped space for memory) with `Ready`/`DisabledByState`/`DeniedByAuthority` verdicts on each action (`src/model.rs::build_pressure`) |
| Activities | this service's live, session-lifetime task groupings (`src/activities.rs`), members joined against the current sample |
| Recovery | stopped processes sampled here, plus the seat report's unresponsive owner ids **joined against those same sampled names** |
| Overview | the CPU and memory readings, the CPU column's line graph fed from a bounded rolling history, each meter carrying the pressure `derive_summary` itself latched |
| Jobs | always empty — see below |

An **activity** is a named grouping of live processes for the current
session, keyed on `proc_id` (single membership; auto-named; inline rename
validated to ≤ 48 trimmed, unique chars; bounds of 12 groups × 32 members
rendered as the controls' disable reasons). Members are pruned — and an
emptied group dissolved — only on a sample whose process list succeeded,
so a degraded sample never wipes groupings; set actions sweep **only
members joined to the current sample**, because a stored numeric pid whose
process exited may have been reused by an unrelated process. A grouping
edit changes the model the panel holds, so it compares unequal to what was
last presented and is drawn once in the same wake, before the service
parks again.

The seat report carries owner **ids only**; the names beside them are the
ones this service attested itself, so display text is never taken from the
wire and an owner this sample never saw contributes no row rather than a
fabricated one. A resource that could not be measured this cycle reads
`unknown` with a `MeterValue::Unmeasured` meter, never a fabricated `0%`.

### Deliberately empty, and why

These are empty because the interfaces that would fill them **do not
exist**, not because they are unfinished:

- **Jobs** — no background-job registry exists anywhere in the OS to
  enumerate.
- **Services** — the System Information API (`lib/abi/src/sysinfo.rs`) has
  no service-enumeration query; its queries cover processes, CPU time, and
  memory pressure.
- **In-panel system actions** — the machine's power transitions are not
  rows *in this window*. They are drawn by the taskbar's quick-actions
  menu, confirmed by the user in the session's modal dialog, and arrive
  here as the `Power` command below, which this service performs under its
  own `CAP_SYSTEM_POWER`. Session lock is the desktop session's own
  surface — it keeps the session running behind it — never this service's.
- **Disk and network pressure cards and resource rows** — no
  disk-throughput query exists at all; a per-interface network-rates query
  exists (`NET_INTERFACE_RATES`) but no tray latch is derived from it, so
  a card would be a guess rather than a measured cause.
- **App "sleep", disk "throttle", activity snapshot/hibernate** (concept
  boards) — no such kernel interfaces exist; the panel offers only actions
  that genuinely work today.

A control that would fail at the point of use is worse than an honest
absence.

### Commands, and who may send them

Commands arrive on the per-instance mailbox `command_endpoint_for(<own
pid>)` this service binds: `OpenPanel { section }`, `SeatReport`, and
`Power { action }`. The session's identity is learned from the reply to
this instance's first accepted publish (`decode_publish_reply`), and every
command is authenticated against the **kernel-attested sender of that very
message**, never a claim on the wire. Dropped with a stated reason, before
the frame is even decoded: a command from any other sender, a command
arriving before any session has been attested, and a frame that does not
decode.

### Actions

| Control | Effect |
|---|---|
| Task row | `SwitchboardRequest::ActivateOwner { owner }` to the session |
| Task *Group* menu | file the task into / out of an activity (service-local state) |
| Pressure *Pause* | `signal(pid, Stop)` on the measured culprit |
| Pressure *Lower priority* | `sched_set_priority(pid, Low)` — renders spent once the record already reads `Low` |
| Pressure *Show tasks* | widget-internal jump to the culprit's task row |
| Activity *Switch* | `ActivateOwner` per joined member, reverse order so the first lands frontmost |
| Activity *Pause*/*Resume* | `signal(pid, Stop)`/`signal(pid, Continue)` swept over joined members; a refusal is reported and the sweep continues |
| Activity *Close* | `signal(pid, Terminate)` swept over joined members (graceful — force-kill stays Recovery's), then the grouping dissolves |
| Recovery *Restart* | `SwitchboardRequest::RestartOwner { owner }` to the session |
| Recovery *Force* | `signal(pid, Kill)` — needs `CAP_PROC_CONTROL` |
| Window *Close* | destroy the window, return to headless sampling |
| `Power` command | `system_power(action)` — needs `CAP_SYSTEM_POWER` |

The desktop session holds no power authority of its own: it is the largest,
most exposed process on the seat, so the widest-blast-radius capability in
the system stays out of it and the confirmed choice is relayed here
instead. This service refuses the transition itself when it does not hold
`CAP_SYSTEM_POWER`, before asking the kernel anything, and the kernel checks
the caller again on the far side of the trap. A granted transition never
returns; a refusal names the transition that did not happen on `stderr` and
leaves the machine running. Every tray summary carries a `power_capable`
flag re-read from this service's own effective set at that moment, so the
taskbar renders those rows refused — never optimistically — whenever the
authority is absent, dropped, or not yet published.

Each control's verdict reflects what this service can *genuinely* do: it
reads its own effective capability set through `cap_query` and compares
each row's kernel-attested owner uid with its own (the same rule the
kernel enforces), and the verdict is re-derived at apply time from the
same inputs so render and enforcement cannot disagree. A control whose
authority is absent renders with the Authority Mark and is never
attempted. A sampled task id that does not fit the syscalls' signed width
is refused, never truncated into a different, arbitrary process. A refusal
from the kernel or the session is stated on `stderr`, leaves the model
untouched, and never ends the service — a refused optional action is an
answer, not a fatal error.

## Capability sizing

`AppInfo.toml` requests exactly `CAP_CONSOLE_WRITE`, `CAP_SYSINFO_GLOBAL`,
`CAP_SYSINFO_KERNEL`, `CAP_SHM` (the zero-copy window frame region the
session maps, as for any windowed app), `CAP_PROC_CONTROL` (signalling a
task this service did not spawn) and `CAP_SYSTEM_POWER` (the machine
transition the session relays here rather than performing itself). The
kernel grants the intersection with the launching user's ceiling — so an
ordinary account's instance simply publishes that it is not power-capable
— and the service probes the two optional sampling scopes **once** at
startup (`probe_scopes`) — capability sets are fixed at spawn, so
re-probing per sample could only rediscover the same answer while spamming
the audit log with denied audited queries:

- an **administrator's** Switchboard sees the system-wide process list and
  the memory-pressure gauge;
- an **ordinary user's** Switchboard degrades cleanly to self-scope: its
  own processes, the overall CPU fraction (ungated), and no memory signal.

Either way the service keeps running and publishing what it can honestly
see; a refused scope is an answer, not a fatal error.

## Cadence and keepalive

The run loop is tickless: **one** `waitset_wait` per iteration, parked with
a timeout equal to the time until the next real sample is due
(`src/schedule.rs`). Sampling is strict: a cycle triggered by an input or
command wake before the deadline is a no-op that never re-queries the
system. That single wait covers every source (`src/wait.rs`):
the termination signal, the command mailbox, the machine's memory-pressure
band, and — only while a window is
open — that window's event mailbox, which joins the set when the window
opens and leaves it when the window closes so a closed window's channel is
never left armed. There is no poll loop and no sleep anywhere.

- **Sample period: 2 s** (`SAMPLE_PERIOD_NS`) — frequent enough that the
  tray reads as live, sparse enough that the ungated per-sample queries
  stay a negligible fraction of system load. Deadlines advance anchored to
  the schedule (not to "now"), so the cadence does not drift by the work
  time of each cycle, and an overdue schedule skips the period it missed
  rather than firing a catch-up burst.
- **Memory cadence: every 5th sample** (`MEMORY_SAMPLE_DIVIDER`, i.e. every
  10 s) — the memory-pressure query is audited per call, so its rate is
  bounded independently of the sample period; the reading is carried
  forward between queries.
- **Keepalive: 10 s** (`KEEPALIVE_NS`) — publication is change-only
  against the last *acknowledged* summary, with a keepalive republish so a
  quiet system still proves the service alive. The keepalive doubles as
  orphan detection: an instance whose session died discovers it, at the
  latest, on its next keepalive attempt.

The periodic re-sample is the sanctioned polling fallback: the system-wide
metrics it reads (process CPU times, aggregate totals, the pressure band)
expose no change event to park on, so the service waits the interval on a
one-shot deadline — the CPU sleeps between samples, and there is no tight
re-poll loop anywhere.

## Lifecycle

Spawned by the desktop session after login (never by PID 1). Startup order
in `src/run.rs`: enable signal intake, learn this process's own
kernel-attested identity (`self_origin`), bind the command and window-event
mailboxes under it, build and arm the wait-set (the termination signal is
both the graceful-exit path and a parking source — failure at any of these
is a stated fatal exit), probe the scopes once, then loop sample → derive →
refresh the panel → offer → publish → park.

Exit rules — every abnormal exit states its reason on `stderr` first:

- **Termination signal** → one terse line naming the signal, exit `0`.
- **Publish refused with `NotFound`** (no session bound the endpoint, or it
  exited) or **`PermissionDenied`** (the session refused this instance —
  e.g. an orphan after a session restart) → a stated **clean** exit `0`:
  the service has no purpose without a session to report to.
- **Publish refused with `WouldBlock`** → back-pressure, not a fault, and
  it costs the service nothing. A call endpoint at capacity refuses the
  post outright rather than blocking, so a full queue says only that the
  session has not drained it yet: the summary stays unacknowledged and the
  change gate re-offers it next sample. Counting it towards the give-up
  budget below let a desktop that was merely busy for five sample periods
  kill the monitor watching it — and nothing restarts one, so the tray
  capsule stayed dead until the user pressed it again after the session had
  reaped the corpse.
- **Any other publish failure** → the summary stays unacknowledged and is
  retried next cycle; after 5 consecutive such faults the service exits
  with a stated reason rather than retrying forever.
- **Wait-set failure** → stated exit: continuing without a real park would
  busy-loop.
- **Command mailbox bind refused** → stated exit: a monitor that can never
  be asked to show its overview should say so rather than run on deaf.

## Dependencies and layering

The library is `no_std` (with `alloc`) and consumes only `tairix-abi` (the
wire vocabulary and `Errno`), `tairix-procinfo` (the shared sysinfo client
helpers), `tairix-controls` (the shared controls its own screen composition
is assembled from), and the crates that screen draws through —
`tairix-geometry`, `tairix-theme`, `tairix-raster`, `tairix-input`, and
`tairix-font` — no kernel or driver crate, no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9, §17.4).
The `Run` binary additionally links `tairix-rt` (the pure-Rust userland
runtime), the window-channel client, and the font crate's glyph-service
transport, for the bare-metal targets only; on the host it is an inert stub
so workspace-wide builds, clippy, and fmt still cover the file. The screen's
own tests additionally take the controls' shared heavy-contrast theme
fixture through the `test-support` feature, so they assert against the
identical fixture every control's own suite uses rather than a private copy.
Nothing outside `userland/gui/*` depends on this crate (`AGENTS.md` §17.3),
so a headless image omits it cleanly.

Everything with behaviour worth testing is host-tested, with the modules
and their tests side by side under `src/`: the sampler against a scripted
in-memory `Transport` fixture, the screen composition against real window
geometry, theme metrics, and font metrics — so a pixel or a scroll offset a
test observes is the one a user would really have — and the whole run-loop
body plus the window lifecycle against a recording `ServiceHost`
(`src/test_host.rs`) whose wait-set bookkeeping mirrors the production
host's, so the membership assertions are real. The `Run` binary is left
holding only the wiring the host cannot run: syscalls, mailboxes, painting,
and wire-event translation.
