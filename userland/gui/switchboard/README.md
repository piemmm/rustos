# tairix-switchboard

The TAIRiX **Switchboard monitor service** (`plans/NEW-TASKBAR.md` T10): the
dedicated, capability-sized process behind the taskbar's always-right-most
Switchboard icon. It samples the live system through the System Information
API and publishes a compact `TraySummary` to the desktop session over the
seat-scoped `SWITCHBOARD_ENDPOINT` (`lib/abi/src/switchboard_ipc.rs`), which
the session binds and the taskbar renders as the tray signals.

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
  processes (the tray's `recovery` signal) and the **top task** — the
  process with the highest CPU-time delta since the previous sample, keyed
  on the stable, never-reused `proc_id` so numeric-pid reuse can never
  stitch two lifetimes together. The first sample honestly has no top task:
  there is no interval to measure over.
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

## Capability sizing

`AppInfo.toml` requests exactly `CAP_CONSOLE_WRITE`, `CAP_SYSINFO_GLOBAL`,
and `CAP_SYSINFO_KERNEL`. The kernel grants the intersection with the
launching user's ceiling, and the service probes the two optional scopes
**once** at startup (`probe_scopes`) — capability sets are fixed at spawn,
so re-probing per sample could only rediscover the same answer while
spamming the audit log with denied audited queries:

- an **administrator's** Switchboard sees the system-wide process list and
  the memory-pressure gauge;
- an **ordinary user's** Switchboard degrades cleanly to self-scope: its
  own processes, the overall CPU fraction (ungated), and no memory signal.

Either way the service keeps running and publishing what it can honestly
see; a refused scope is an answer, not a fatal error.

## Cadence and keepalive

The run loop is tickless: one `waitset_wait` per iteration, parked with a
timeout equal to the time until the next thing that must happen
(`src/schedule.rs`), and woken early only by a termination signal.

- **Sample period: 2 s** (`SAMPLE_PERIOD_NS`) — frequent enough that the
  tray reads as live, sparse enough that the ungated per-sample queries
  stay a negligible fraction of system load. Deadlines advance anchored to
  the schedule (not to "now"), so the cadence does not drift by the work
  time of each cycle, and an overdue schedule resyncs rather than firing a
  catch-up burst.
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
in `src/run.rs`: enable signal intake and build the one-member wait-set
(the termination signal is both the graceful-exit path and the parking
source — failure here is a stated fatal exit), probe the scopes once, then
loop sample → derive → offer → publish → park.

Exit rules — every abnormal exit states its reason on `stderr` first:

- **Termination signal** → one terse line naming the signal, exit `0`.
- **Publish refused with `NotFound`** (no session bound the endpoint, or it
  exited) or **`PermissionDenied`** (the session refused this instance —
  e.g. an orphan after a session restart) → a stated **clean** exit `0`:
  the service has no purpose without a session to report to.
- **Any other publish failure** → the summary stays unacknowledged and is
  retried next cycle; after 5 consecutive failures the service exits with
  a stated reason rather than retrying forever.
- **Wait-set failure** → stated exit: continuing without a real park would
  busy-loop.

## Dependencies and layering

The library is `no_std` (with `alloc`) and consumes only `tairix-abi` (the
wire vocabulary and `Errno`) and `tairix-procinfo` (the shared sysinfo
client helpers) — no kernel or driver crate, no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9, §17.4).
The `Run` binary additionally links `tairix-rt` (the pure-Rust userland
runtime) for the bare-metal targets only; on the host it is an inert stub
so workspace-wide builds, clippy, and fmt still cover the file. Nothing
outside `userland/gui/*` depends on this crate (`AGENTS.md` §17.3), so a
headless image omits it cleanly.

The sampler/derive/publish/schedule core is host-tested against a scripted
in-memory `Transport` fixture; the modules and their tests live side by
side under `src/`.
