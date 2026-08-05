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
session's liveness vigil finds unresponsive), and `Power` (the machine
transition the user confirmed in the taskbar's quick-actions menu — see
[Power transitions](#power-transitions)). Every command is authenticated
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

The window's own chrome is the standard frame and title bar plus a **location
band** under them: a `Breadcrumb` reading `Switchboard › <section>` and, at
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
| Tasks | the sampled process list; each row's primary action asks the session to raise that owner's window, and its `Group` button files the task into an activity (below) |
| Pressure | "why is my machine slow": one cause card per resource the **tray's own latches** flag (CPU ≥ 90 % with < 80 % release; memory band ≥ mild), naming the measured culprit — the busiest sampled task for CPU, the largest mapped address space for memory — with a plain-language cause line and recommended actions, each rendered `Ready`, `DisabledByState` (a culprit already at `Low` priority or already stopped) or `DeniedByAuthority` per the same rule the kernel enforces. No latch, no card; no per-task rate yet, a culprit-less card with the one action that is still honest (`Show tasks`) |
| Activities | the service's own **session-lifetime task groupings**: named sets of live processes (keyed by the never-reused `proc_id`), each rendered with its member rows joined against the current sample. Created from a task row's `Group` menu; renamed inline; paused/resumed/closed as a set |
| Recovery | stopped processes this service sampled itself, plus the seat report's unresponsive owner ids **joined against those same sampled names** — the report carries ids only, so an owner this service never saw produces no row rather than a fabricated one |
| Overview | the CPU and memory readings, with the CPU column's line graph fed from a bounded rolling history and each meter carrying the pressure the tray derivation itself latched |
| Jobs | always empty — see below |

A resource the service could not measure this cycle reads `unknown` with an
unmeasured meter, never a fabricated `0%`.

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
  enumerate.
- **Services** — the System Information API (`lib/abi/src/sysinfo.rs`) has
  no service-enumeration query; its queries cover processes, CPU time, and
  memory pressure.
- **In-panel system actions** — the machine's power transitions are not
  rows *in this window*: they live in the taskbar's quick-actions menu,
  where the user confirms them, and reach this service as the `Power`
  command below. Session lock is the desktop session's own surface (it
  keeps the session running behind it), never this service's.
- **Disk and network pressure cards and resource rows** — the API exposes
  no disk-throughput query at all, and while a per-interface network-rates
  query exists (`NET_INTERFACE_RATES`), no tray latch is derived from it,
  so a card would be a guess rather than a measured cause.
- **App "sleep" and disk "throttle"** (sketched on the concept boards) —
  no such kernel interfaces exist; the pressure cards offer only actions
  that genuinely work today: pause (`Stop`), lower priority, show tasks.

Offering a control that would fail at the point of use is worse than an
honest absence, so these stay empty.

### Actions

| Control | Effect |
|---|---|
| Task row | `SwitchboardRequest::ActivateOwner { owner }` to the session |
| Task *Group* menu | file the task into an activity / a new activity / out of its activity (service-local state; no syscall) |
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
covers the termination signal, the command mailbox, and — only while a
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
`CAP_SYSINFO_KERNEL`, `CAP_SHM` (the zero-copy window frame region the
session maps, as for any windowed app), `CAP_PROC_CONTROL` (delivering a
control signal to a task this service did not spawn — the Force action), and
`CAP_SYSTEM_POWER` (the machine transition the session relays here rather
than performing itself); the kernel intersects them with the launching
user's ceiling at spawn, so an ordinary account's instance simply publishes
that it is not power-capable and the desktop's power rows stay refused. The
two optional sampling scopes are probed **once** at startup (capability sets
are fixed at spawn; re-probing would only spam the audit log with denied
audited queries):

- an administrator's instance sees the system-wide process list and the
  memory-pressure gauge;
- an ordinary user's instance degrades cleanly to self-scope — its own
  processes, the ungated overall CPU fraction, no memory signal.

A refused scope is an answer, not an error; the service publishes what it
can honestly see.

## Lifecycle

Started by the desktop session after login; never by PID 1. Exits — each
with its reason stated on `stderr` first:

- a termination signal → clean exit;
- `NotFound` / `PermissionDenied` from the endpoint (no session, or the
  session refused a stale instance) → clean exit — the service has no
  purpose without a session to report to;
- five consecutive publish failures, or a wait-set failure → a stated
  abnormal exit rather than an unbounded silent retry or a busy loop.

Design details and constants live in the crate's rustdoc and
`userland/gui/switchboard/README.md`.
