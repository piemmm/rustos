# Switchboard monitor service

`userland/gui/switchboard` (`tairix-switchboard`) is the **Switchboard
monitor service** (`plans/NEW-TASKBAR.md` T10/T11): a small, dedicated
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
the overview on a named section) and `SeatReport` (which window owners the
session's liveness vigil finds unresponsive). Every command is authenticated
against the kernel-attested sender of that very message, never a claim on
the wire; a command from anyone but the attested session, a command that
arrives before any session has been attested, and a frame that does not
decode are each dropped with a stated reason and never touch the model.

## The live overview window

`OpenPanel` shows the shared `Switchboard` composition
(`lib/controls/src/switchboard.rs`) on the requested section, through
`Switchboard::select_section`. There is at most **one** window: a second
`OpenPanel` asks the session to raise the existing one (naming this
service's own pid) and switches section rather than stacking a second. The
window's close control destroys it and the service returns to headless
sampling — sampling and publishing continue unchanged whether or not a
window is open, because the window is a view onto a monitor that never stops
monitoring.

The model is rebuilt on the same sample cadence and re-rendered only when it
actually changed. What it carries:

| Section | Source |
|---|---|
| Tasks | the sampled process list; each row's action asks the session to raise that owner's window |
| Recovery | stopped processes this service sampled itself, plus the seat report's unresponsive owner ids **joined against those same sampled names** — the report carries ids only, so an owner this service never saw produces no row rather than a fabricated one |
| Overview | the CPU and memory readings, with the CPU meter's sparkline fed from a bounded rolling history and each meter carrying the pressure the tray derivation itself latched |
| Jobs | always empty — see below |

A resource the service could not measure this cycle reads `unknown` with an
unmeasured meter, never a fabricated `0%`.

### What is deliberately empty, and why

- **Jobs** — there is no background-job registry anywhere in the OS to
  enumerate.
- **Services** — the System Information API (`lib/abi/src/sysinfo.rs`) has
  no service-enumeration query; its queries cover processes, CPU time, and
  memory pressure.
- **System actions** — there is no power or session-lock interface this
  service may drive, so it offers no shut-down, restart, or lock button.
- **Disk and network resource rows** — the System Information API exposes
  no throughput query for either.

Offering a control that would fail at the point of use is worse than an
honest absence, so these stay empty.

### Actions

| Control | Effect |
|---|---|
| Task row | `SwitchboardRequest::ActivateOwner { owner }` to the session |
| Recovery *Restart* | `SwitchboardRequest::RestartOwner { owner }` to the session |
| Recovery *Force* | `signal(pid, Kill)` — requires `CAP_PROC_CONTROL` |
| Window *Close* | destroy the window, return to headless sampling |

Every row's availability reflects what this service can *genuinely* do: it
queries its own effective capability set through `cap_query`, and a control
whose authority is absent renders with the Authority Mark and is never
attempted. A sampled task id that does not fit the `signal` syscall's signed
width is refused rather than truncated into a different, arbitrary process.
A refusal from the kernel or the session is stated on `stderr` and leaves
the model untouched; it never ends the service.

## Waiting

The loop is tickless and event-driven: **one** `waitset_wait` per iteration
covers the termination signal, the command mailbox, and — only while a
window is open — that window's event mailbox, with a timeout equal to the
time remaining until the next sample is due. There is no poll loop and no
sleep. The window's event source joins the set when the window opens and
leaves it when the window closes, so a closed window's channel is never left
armed. The periodic re-sample is the documented polling fallback: the
system-wide metrics expose no change event to park on.

## Capability sizing

The manifest requests exactly `CAP_CONSOLE_WRITE`, `CAP_SYSINFO_GLOBAL`,
`CAP_SYSINFO_KERNEL`, `CAP_SHM` (the zero-copy window frame region the
session maps, as for any windowed app), and `CAP_PROC_CONTROL` (delivering a
control signal to a task this service did not spawn — the Force action); the
kernel intersects them with the launching user's ceiling at spawn. The two
optional sampling scopes are probed **once** at startup (capability sets are
fixed at spawn; re-probing would only spam the audit log with denied audited
queries):

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
