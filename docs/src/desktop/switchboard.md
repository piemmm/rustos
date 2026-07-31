# Switchboard monitor service

`userland/gui/switchboard` (`tairix-switchboard`) is the **Switchboard
monitor service** (`plans/NEW-TASKBAR.md` T10): a small, dedicated process
the desktop session spawns as the logged-in user, which samples the live
system through the System Information API and feeds the taskbar's
always-right-most Switchboard icon its tray signals.

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
detection. The loop is tickless: one wait per iteration, parked until the
next deadline or a termination signal — never a busy poll. The periodic
re-sample is the documented polling fallback: the system-wide metrics
expose no change event to park on.

## Capability sizing

The manifest requests exactly `CAP_CONSOLE_WRITE`, `CAP_SYSINFO_GLOBAL`,
and `CAP_SYSINFO_KERNEL`; the kernel intersects them with the launching
user's ceiling at spawn. The two optional scopes are probed **once** at
startup (capability sets are fixed at spawn; re-probing would only spam the
audit log with denied audited queries):

- an administrator's instance sees the system-wide process list and the
  memory-pressure gauge;
- an ordinary user's instance degrades cleanly to self-scope — its own
  processes, the ungated overall CPU fraction, no memory signal.

A refused scope is an answer, not an error; the service publishes what it
can honestly see.

## Lifecycle

Started by the desktop session after login; never by PID 1. It parks on a
one-member wait-set whose termination-signal member is both the graceful
exit path and the parking source. Exits — each with its reason stated on
`stderr` first:

- a termination signal → clean exit;
- `NotFound` / `PermissionDenied` from the endpoint (no session, or the
  session refused a stale instance) → clean exit — the service has no
  purpose without a session to report to;
- five consecutive publish failures, or a wait-set failure → a stated
  abnormal exit rather than an unbounded silent retry or a busy loop.

Design details and constants live in the crate's rustdoc and
`userland/gui/switchboard/README.md`.
