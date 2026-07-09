# STRESSTEST.md — System stress testing and live kernel monitoring

Status: planned (ST1–ST6)
Target: RustOS
Primary code areas: `lib/abi`, `kernel/mem`, `kernel/core`, `userland/system/sysinfod`, `lib/procinfo`, `userland/apps/`
Secondary code areas: `lib/rt`, `lib/curses`, `userland/shell/sysinfo`, `kernel/sched`, `docs/src/`

This is the staged build plan for RustOS's stress-testing and
system-monitoring tier:

- **`sysmon`** — a fullscreen curses monitor application that marks its own
  memory unswappable and observes every aspect of the kernel's memory:
  physical memory, kernel heap, per-process residency, the reclaimable
  caches, the `ramzip` compressed tier, memory-pressure bands, reserves,
  and per-CPU load — usable while the system is under deliberate stress
  (its primary function) and at idle.
- **`stress`** — a comprehensive load-generation command app in the spirit
  of the established `stress`/`stress-ng` command surface: an unswappable
  controlling process spawning swappable workers that load the CPU, memory,
  disk, I/O, and the kernel caches — separately or combined, with a
  configurable overcommit level, signal/Ctrl-C handling, quiet and
  background modes, a run timeout, and an option to run `sysmon` alongside.
- The **observability plumbing** both need: new capability-gated System
  Information API queries (memory detail, pressure, reclaim ledger,
  `ramzip`, caches, per-CPU load) with matching `info:` / `stats:`
  resource-reference selectors (`info:cpu/...`, `stats:mem/pressure`, …)
  served by `sysinfod` and resolved by `lib/procinfo`.
- The two **kernel mechanisms** neither program can be built without: a
  capability-gated, resource-limited memory-pinning surface (the real API
  behind `plans/SWAPSWAPSWAP.md` §5's "pinned" eligibility class), and a
  fail-closed signal-observation opt-in so the stress controller can catch
  `Interrupt`/`Terminate` and tear its workers down before exiting.

`AGENTS.md` is binding — read it and `PLAN.md` first. Every rule in this
file is binding too. This is a design, invariant, staging, and acceptance
document, not a changelog and not a merge-ready implementation. If this
document conflicts with `AGENTS.md`, `AGENTS.md` wins. If it conflicts with
the repository's actual state, the implementing agent must surface the
mismatch before implementing a guess.

Related plans: `plans/SWAPSWAPSWAP.md` (the `ramzip` tier this observes and
whose pinned-page class ST2 realises), `plans/SMARTRAM.md` (the pressure
gauge and reclaimable caches this observes), `plans/SPAWN.md` (spawn,
`wait`, the `Signal` set, foreground delivery), `plans/APPS.md` (command-app
packaging, options, help), `plans/CURSES.md` (the TUI stack `sysmon` draws
through), `plans/ALIAS.md` (the `info:`/`stats:` reference grammar),
`plans/CAPABILITY_USE.md` (capability lifecycle), `plans/SYSLOG.md` (audit
events).

---

## 0. Starting point (what already exists)

Facts the stages below build on, so no stage re-derives them:

- **The System Information API is live and extensible.**
  `lib/abi/src/sysinfo.rs` defines the frozen-on-release `SYSINFO_QUERIES`
  registry (13 queries today, ids 0–12) with per-query capability + audit
  flags; `userland/system/sysinfod` is the sole dispatcher (decode → spec
  lookup → capability check before state → audit → answer through the
  injected `SysinfoSource` seam). `KERNEL_MEMORY_STATS`
  (`CAP_SYSINFO_KERNEL`, audited) is deliberately coarse: total/free/
  kernel-heap/user-resident/page-size only. `CPU_TIME_STATS`,
  `LOAD_AVERAGE`, `UPTIME`, and the process lists exist.
- **The `info:`/`stats:` resolver is live.** `lib/resref` owns the closed
  16-namespace registry (`info`, `stats`, `state`, …);
  `lib/procinfo::resolve` maps parsed references onto sysinfo queries and
  serves today: `info:hostname|kernel/version|machine-id|boot-time|
  process/*|mem/physical|mem/page-size|limits/*` and `stats:uptime|
  mem/{used,available,kernel-heap,user-resident}|limits/*`. There is no
  `info:cpu`, no `stats:cpu/*`, no `stats:mem/pressure`, and nothing for
  reclaim, caches, or `ramzip`.
- **The kernel already keeps the numbers; it just does not export them.**
  `kernel/mem::pressure` is the shared five-band gauge with hysteresis and
  per-band transition counters; `kernel/mem::reclaim` is the classified
  ledger (per-class payload/metadata bytes, shrink/teardown/failure
  counters); `kernel/mem::ramzip` accounts caps, stored/logical bytes,
  compression attempts/acceptances/rejections, fault-ins, authentication
  failures, and thrash escalations (`plans/SWAPSWAPSWAP.md` §10); the
  SMARTRAM caches (`CachedFs`, the RustFS `TransformClusterCache`, the
  `LaunchCache`, the `BlockCache`) are budgeted and counted. All of it is
  internal-only today (`plans/SMARTRAM.md` SMART9 — "no public ABI"), and
  `plans/SWAPSWAPSWAP.md` §16 already rules that a public query belongs in
  the System Information API with full ABI discipline. One caveat carries
  through this whole plan: enabling `ramzip` for arbitrary *running* tasks
  awaits the restartable-user-page-fault prerequisite staged in `PLAN.md`
  (every port's user fault hook is terminal today) — the queries report
  whatever the tier really does, and the assertions that stress makes its
  counters *move* bind only once that prerequisite has landed.
- **Signals exist; catching them does not.** `rustos_abi::Signal` is the
  closed set `Continue`/`Terminate`/`Kill`/`Interrupt`/`Stop`
  (`plans/SPAWN.md` SP9, landed): the console line discipline delivers
  `^C`/`^Z` to the marked foreground child, `wait` reports stops, elsh has
  `jobs`/`fg`/`bg`, and the `signal` syscall lets a parent signal its own
  live children (unprivileged for one's own child, audited per call).
  Every terminating signal's disposition is fixed: `Interrupt` terminates
  with status 130. No process can observe or handle a signal today.
- **Pinning is a named hole.** `plans/SWAPSWAPSWAP.md` §5 makes "pinned"
  pages ineligible for `ramzip` and names the marker "a future approved
  API"; no `mem_pin`/`mlock`-equivalent syscall, capability, or resource
  limit exists anywhere in the tree yet.
- **The TUI template is `top`.** `userland/apps/top` is the model: a full
  command-app bundle (AppInfo, `Help/` locales, `Run.ld`) drawing through
  `lib/curses` with an I/O-free `model` + `app` split, event-driven loop,
  alternate screen, and capability refusals rendered as refusals while the
  session continues (fail closed, degrade gracefully).
- **Timed waiting exists.** `waitset_wait` takes a relative nanosecond
  timeout (`u64::MAX` = none) over owner-checked members; `clock_get`
  provides the monotonic clock. Nothing needs to spin to wait
  (`AGENTS.md` §2.23).
- **Resource limits exist.** `rlimit_get`/`rlimit_set` and the `ulimit`
  builtin implement the §24.3 facility with typed `LimitKind`s,
  soft/hard bounds, and the capability gate on raising a hard bound.

---

## 1. Goal

Make RustOS's behaviour under load *observable* and *provokable* with
first-party tools: an operator (or CI vertical) starts `stress` to drive
the machine into any chosen combination of CPU, memory, disk, I/O, and
cache pressure, watches the kernel respond in `sysmon` — pressure bands
moving, caches shrinking, `ramzip` filling, reserves holding — and gets
both programs back cleanly on Ctrl-C, a signal, or the timeout. Both
programs survive the very pressure they are built for: their controlling
state is pinned so the monitor never stalls on its own page-fault-in and
the stress controller can always tear its workers down.

## 2. Non-goals

- **Not a benchmark suite.** `stress` loads the system; it does not score
  it. Scored micro-benchmarks stay in the per-crate benches (§7).
- **No network stress.** The userland network stack is a single ICMP
  responder today; network load generation is staged with the future
  network work, not invented here.
- **No thermal/battery/frequency observation.** No such kernel signals
  exist; fabricating them is speculative surface (§2.3/§2.4).
- **No `/proc`, no text-scrape files, no new ad-hoc syscall diagnostics**
  (§16.6 — the System Information API is the only window).
- **No GUI panels.** `sysmon` is a text app; a graphical monitor is a
  future desktop app over the same queries.
- **No per-region pinning ABI.** ST2 pins a whole process's anonymous
  memory; a byte-range `mlock` analogue waits for a real consumer
  (§2.3/§2.4).

---

## 3. Design — observability (ST1)

### 3.1 New sysinfo queries

Four new typed queries join `SYSINFO_QUERIES`, all under the **existing**
`CAP_SYSINFO_KERNEL` (they expose kernel-wide operational statistics — the
same security boundary `KERNEL_MEMORY_STATS` already guards; §5.2 forbids a
new capability where an existing one expresses the authority), all audited
like their sibling:

- **`MEMORY_PRESSURE`** — the live pressure band (the shared
  `kernel/mem::pressure` five-band gauge), the band thresholds actually in
  force (derived values, reported not promised), the reserve floor, free
  frames, and the per-band transition counters since boot.
- **`RECLAIM_STATS`** — the `kernel/mem::reclaim` ledger paged per class:
  class id, payload bytes, metadata bytes, entry count, shrink/teardown
  counters, refusal counters. The SMARTRAM caches (filesystem cache,
  transform cache, launch cache, block cache) appear here as the classes
  they already are — one ledger, not a second per-cache query.
- **`RAMZIP_STATS`** — the SWAPSWAPSWAP §10 accounting dimensions: stored
  encrypted bytes, logical bytes represented, metadata bytes, min/soft/hard
  cap usage, compression attempts/acceptances/rejections, fault-ins,
  authentication failures, decompression failures, warm-up counters,
  thrash escalations, and per-boot pinned-exemption counts (ST2). Never
  page contents, never key material — counters only
  (`plans/SWAPSWAPSWAP.md` §16).
- **`CPU_LOAD`** — per-CPU figures paged per CPU: cpu id, busy/idle
  cumulative time (`Duration64`), the run-queue depth sample, and the
  context-switch and preemption counters the scheduler already keeps.
  The implementing agent first reconciles against `CPU_TIME_STATS`: if
  that query already carries a needed figure, `CPU_LOAD` carries only the
  remainder — the same number is never served twice (§2.2).

Rules carried over unchanged: typed request/response with `WIRE_LEN`
encode/decode and fail-closed `from_bytes` (reserved fields must be zero),
whole-record paging like `MOUNT_LIST`/`SEAT_LIST`, `Time64`/`Duration64`
for every time, `u64` for every byte count (§21, §26.6), fuzz targets for
every new decoder (§19.6), and the `sysinfod` dispatcher changes nothing
structurally — new rows in the registry, new arms in the `SysinfoSource`
seam.

### 3.2 New `info:` / `stats:` selectors

`lib/procinfo::resolve` gains, over the new queries (namespaces are the
existing closed `info`/`stats` set — nothing widens `lib/resref`):

- `info:cpu/count` — the number of online CPUs (gated like
  `info:mem/physical`, a kernel-tier fact).
- `stats:cpu/load` — the all-CPU busy share over the sampling window the
  resolver computes from two `CPU_LOAD` reads; `stats:cpu/<n>/load` the
  per-CPU form. `stats:cpu/switches` — cumulative context switches.
- `stats:mem/pressure` — the current band as a small integer gauge with
  the band name in the envelope; `stats:mem/pressure/transitions` — the
  transition counter.
- `stats:mem/reclaim/total`, `stats:mem/reclaim/<class>` — reclaimable
  bytes held, total and per class (class names are the reclaim ledger's
  own stable names).
- `stats:mem/ramzip/{stored,logical,saved}` — stored encrypted bytes,
  logical bytes represented, and their difference.
- `stats:mem/pinned` — pinned bytes system-wide (ST2).

Unknown leaves keep failing closed (`UnknownSelector`); capability denials
keep mapping to `CapabilityDenied`; decorations stay unserviceable. The
`sysinfo` CLI (`userland/shell/sysinfo`) gains matching subcommands so the
terminal surface and the resolver stay one vocabulary. The `state:`
namespace remains owned by `plans/ALIAS.md`'s own staging and is not
colonised here.

---

## 4. Design — memory pinning (ST2)

The "marks itself as unswappable" requirement, and the real API behind the
`ramzip` eligibility class that already treats pinned pages as ineligible
(`plans/SWAPSWAPSWAP.md` §5).

- **Surface.** Two new syscalls, `mem_pin` / `mem_unpin`, taking no
  arguments beyond the implicit kernel-attested caller: they mark/unmark
  the **calling process's entire anonymous memory** — current and future —
  as pinned. Pinned pages are ineligible for the `ramzip` tier and any
  future lower swap tier; the flag is process-scoped state in `kernel/mem`
  consulted by the one eligibility classifier (never a second check).
  Pinning is not inherited across `spawn` (a stress worker starts
  unpinned) and is cleared on exit.
- **Capability.** New `CAP_MEM_PIN`, introduced in the same change as its
  enforcement point (the syscall dispatcher gate) and its live holders
  (`sysmon` and `stress` manifests) — §5.2's three-part test holds: it
  guards a class of operations (exempting memory from reclaim/compression
  system-wide is a denial-of-service lever against every other tenant),
  it lands with holder and enforcement together, and no existing
  capability expresses "may exempt memory from pressure management".
  Both syscalls are audited (a pin is a security-relevant resource
  decision, §19.4).
- **Bounded.** A new `LimitKind::PinnedMemory` joins the §24.3 resource
  limits: the effective limit caps the bytes a process may hold pinned;
  crossing it fails the *allocation or pin* closed with a typed error,
  never a panic. Default policy is a function of discovered RAM (§24.1),
  sized so a monitor-scale process always fits and an abusive pin cannot
  starve the machine; limits intersect across spawn as ever.
- **Fail-closed, degrade-gracefully consumers.** A refused pin
  (capability or limit) is an *answer*: `sysmon` and the `stress`
  controller report the refusal on their UI/stderr and continue unpinned
  (§2.24) — pinning is incidental to their purpose, not fatal.
- **What pinning is not.** It does not lock pages against being paged in
  lazily, does not change zero-on-free or encryption guarantees, does not
  make the process unkillable, and grants no residency promise beyond
  "never enters a swap tier". `stats:mem/pinned` and `RAMZIP_STATS`
  expose the aggregate so an operator can see pinning pressure.

---

## 5. Design — signal observation (ST3)

The stress controller must catch `^C` (`Signal::Interrupt`) and
`Terminate`, tear its workers down, restore the terminal line, and exit
130/143 — today those signals terminate it before it can act.

- **Surface.** One new syscall, `signal_intake`, by which a process opts
  its **own** termination-request signals (`Interrupt` and `Terminate`
  only) out of default-terminate and into delivery as an observable
  event. No handler trampoline, no asynchronous user-mode re-entry (that
  machinery has no other consumer and huge attack surface): the pending
  signal becomes a waitable **signal source** the process adds to a
  waitset (`waitset_ctl` gains the `Signal` source kind beside
  `Endpoint`/`Irq`) or drains explicitly — the event-driven shape every
  other waiter already has (§2.23).
- **What is never observable.** `Kill` remains unconditionally fatal and
  unmaskable. `Stop`/`Continue` remain scheduler-side. A process that has
  opted in and then never drains its intake does not become immortal:
  a second `Interrupt` arriving while one is already pending undelivered
  escalates to the default terminate path — an unresponsive program stays
  killable with plain `^C ^C`, no new capability, no privileged override
  needed.
- **Scope.** Own-process disposition needs no capability (the same tier
  as `stream_input_mode`); the opt-in and each observed delivery are
  audited like `signal` itself. `lib/rt` gains the safe wrappers; the
  C ABI stubs follow automatically from the generated table.
- The elsh side needs no change: the console line discipline and
  foreground marking (`plans/SPAWN.md` SP9) already deliver the signal;
  only the *target's disposition* changes.

---

## 6. Design — `sysmon` (ST4)

A system app-store command app (`userland/apps/sysmon`), packaged and
built exactly like `top` (full self-contained bundle, `Help/` tree,
model/app split over `lib/curses`, alternate screen so the shell's screen
is restored on exit).

- **Startup.** Parses options, pins itself (`mem_pin`; a refusal is
  rendered, not fatal), enters the alternate screen, and starts the
  event-driven loop: one waitset over stdin input and the refresh
  deadline (`waitset_wait` with the interval timeout — never a poll
  loop).
- **Panels** (all figures from the sysinfo queries, never a private
  kernel channel):
  - *Memory overview* — total/free/kernel-heap/user-resident
    (`KERNEL_MEMORY_STATS`), pinned bytes, page size.
  - *Pressure* — the current band, rendered as name + gauge, with a
    scrolling band history strip and the transition counters
    (`MEMORY_PRESSURE`).
  - *Reclaimable caches* — the per-class ledger table with payload/
    metadata bytes and shrink/refusal counters (`RECLAIM_STATS`).
  - *Compressed tier* — `ramzip` stored/logical/saved bytes, cap usage
    against min/soft/hard, compression/fault-in/thrash counters
    (`RAMZIP_STATS`).
  - *CPU* — per-CPU busy% over the refresh interval, run-queue depth,
    context switches (`CPU_LOAD`), and the 1/5/15 load averages
    (`LOAD_AVERAGE`).
  - *Processes (summary)* — the census by state and the top consumers by
    `%CPU` and by resident bytes (the existing process-list queries; the
    full interactive list remains `top`'s job — no duplication).
- **Keys.** `q` quit, `r` refresh now, `+`/`-` change the refresh
  interval within a bounded range, `p` cycle panel focus/scroll, `?`
  help overlay — the `top` conventions.
- **Degradation.** Each gated query that is refused renders as the
  refusal it is in its panel while the rest of the session continues
  (§5.4, §2.24); at idle the app is quiescent between deadlines (no
  busy redraw). It works identically over serial, the video console,
  or a future WM terminal — fd 0/1/2/3 only (§20).
- **Capabilities requested** (manifest): `CAP_SYSINFO_KERNEL`,
  `CAP_SYSINFO_GLOBAL` (all-process summary; optional, degrades to
  own-process), `CAP_MEM_PIN`.

---

## 7. Design — `stress` (ST5)

A system app-store command app (`userland/apps/stress`) following the
established `stress`/`stress-ng` option surface (§16.7 familiarity —
divergences only where RustOS genuinely differs, documented in `Help/`).

### 7.1 Process model

One **controller** process (pins itself via `mem_pin`, opts into
`signal_intake`) spawning N **worker** children (unpinned — deliberately
swappable, so memory workers exercise `ramzip`), one per requested load
unit, wired through the existing `spawn` startup-strings block (each
worker is the same binary re-entered in worker mode via argv — no second
executable). The controller's loop is one waitset: child exits (`wait`),
the signal intake, and the timeout deadline. On `Interrupt`/`Terminate`/
timeout it signals every live worker `Terminate` (then `Kill` after a
bounded grace deadline), reaps them all, prints the summary (unless
`--quiet`), and exits with the §16.7-familiar status (0 on clean
completion, 130/143 on signals).

### 7.2 Load subsystems

Each is a worker kind with a bounded, restartable unit of work; a worker
that hits a typed refusal (rlimit, ENOSPC, capability) reports it once on
stderr and exits — the controller counts and reports refusals, never
retry-until-it-works (§2.1):

- **cpu** — tight arithmetic loops (integer/float mix) that never issue a
  syscall: exercises preemption (§17.1 — the conformance suite's
  CPU-bound-task property, now provokable on demand).
- **vm** — allocate/touch/re-touch anonymous memory in a rotating
  pattern sized by `--vm-bytes`, driving allocation, fault, and — once the
  restartable-user-page-fault prerequisite has enabled the tier for
  running tasks (§0) — `ramzip` compress-out and fault-in.
- **io** — stream-write/read/rewrite through the filesystem layer with
  small buffers and frequent `fs_sync`: exercises the write path and the
  block cache.
- **hdd** — large sequential file write/verify/delete cycles sized by
  `--hdd-bytes`: exercises throughput and free-space accounting.
- **cache** — repeated cold directory walks and file re-reads over the
  scratch tree: churns the filesystem/block caches so their ledgers move.

Disk-touching workers write **only** beneath a scratch directory the
invoking user can write: default the app-scoped per-user cache directory
(`Library/stress/` under the invoking user, per §16.5's app-write rule),
overridable with `--temp-path <dir>`; every scratch file is removed on
teardown, including the signal paths.

### 7.3 Options (the closed v1 set)

```
--cpu N          N CPU workers
--vm N           N memory workers        --vm-bytes B   (default sized from discovered RAM)
--io N           N I/O workers
--hdd N          N disk workers          --hdd-bytes B
--cache N        N cache-churn workers
--all N          N workers of every kind
--overcommit P   scale the vm/hdd byte targets to P percent of the
                 discovered resource (RAM for vm, free space for hdd);
                 P may exceed 100 — the workers push into pressure and
                 treat the resulting typed refusals as expected outcomes
--timeout T      stop after T (s/m/h suffixes, e.g. 5m); no default
--monitor        run sysmon in the foreground for the duration; the
                 stress run is reported when the monitor exits
--quiet, -q      suppress the summary and progress lines (errors still
                 reach stderr)
--background     print the controller PID and detach from the terminal
                 discipline so the shell prompt returns (the elsh `&`
                 job form works too; this flag is for scripts) — implies
                 --quiet
--help, --version   per §16.7
```

`--monitor` spawns the installed `sysmon` bundle as a foreground child
with the terminal (never an embedded copy — one monitor implementation,
§2.2); with `--background` it is refused as contradictory (typed usage
error). Everything it can see is what the user's own capabilities allow —
`stress` requests only `CAP_MEM_PIN` beyond the baseline; loading a
machine needs no privilege beyond what the caller's own resource limits
permit, and the limits are the defence (§24.3): `stress` respects them
and reports refusals rather than asking for more.

---

## 8. Staged implementation plan

Each stage lands as a complete, tested, documented, fully-gated change
(fmt, `cargo xtask ci` once, `fuzz --secs 5`, the capped developer soak),
with rustdoc, the mdBook pages, and `PLAN.md` updated in the same change.
No stubs, no `todo!()`, no deferred tests.

### ST1 — Observability queries and selectors

Deliverables: the four queries of §3.1 in `lib/abi` + `sysinfod` +
kernel `SysinfoSource` plumbing; the §3.2 selectors in `lib/procinfo`;
`sysinfo` CLI subcommands; fuzz targets for every new decoder.
Tests: encode/decode round-trips and malformed-input rejection per type;
dispatcher capability-denial and audit tests against the fixture source;
resolver selector tests (gated gauge, denial mapping, unknown leaf fails
closed); a kernel host test that the exported counters move when the
gauges/ledgers move.

### ST2 — Memory pinning

Deliverables: `mem_pin`/`mem_unpin` (+ generated table/C headers),
`CAP_MEM_PIN` with manifest grants, `LimitKind::PinnedMemory` with a
discovered-RAM default policy, the `kernel/mem` process-pin state wired
into the one `ramzip` eligibility classifier, `lib/rt` wrappers,
`stats:mem/pinned` (extends ST1's payloads, reserved-field evolution).
Tests: pin honoured (pinned pages never selected under simulated
pressure), unpin restores eligibility, limit crossing fails closed,
capability denial fails closed, no inheritance across spawn, cleared on
exit, audit events emitted; host + QEMU pressure vertical.

### ST3 — Signal observation

Deliverables: `signal_intake`, the waitset `Signal` source kind, the
double-`Interrupt` escalation rule, `lib/rt` wrappers, docs on the
disposition model.
Tests: opted-in `Interrupt` is observed not fatal; `Kill` still kills an
opted-in process; second pending `Interrupt` escalates to terminate;
un-opted process keeps SP9 behaviour byte-for-byte; foreground `^C`
through the real console line discipline reaches the intake (QEMU
vertical); audit events.

### ST4 — `sysmon`

Deliverables: the `userland/apps/sysmon` bundle (AppInfo, `Help/` with
`en-US` plus the locale set the sibling apps carry, README, docs page);
the §6 panels/keys; pin-on-start with graceful refusal; event-driven
refresh loop.
Tests: exhaustive I/O-free model tests (panel state, key handling,
degradation states); render tests against fixture query data; a QEMU
vertical that starts `sysmon` on the console, drives a refresh, asserts
band/counter figures render, and quits back to an intact shell screen.

### ST5 — `stress`

Deliverables: the `userland/apps/stress` bundle; controller/worker model
(§7.1), the five subsystems (§7.2), the option set (§7.3) with
fail-closed parsing; scratch-tree hygiene on every exit path; the
summary report; stdinfo `summary` record on fd 3 (§20.1).
Tests: option-parser unit tests (valid/invalid/contradictory); worker
unit tests against seams (bounded work units, refusal handling); teardown
tests (Interrupt → workers terminated and reaped, scratch removed, exit
130; timeout → same with exit 0; Kill of a worker → controller notices
and reports); QEMU verticals: a short `--all` run under `--timeout`
moves `MEMORY_PRESSURE`/`RAMZIP_STATS` counters and returns the prompt
clean; `^C` mid-run tears down.

### ST6 — Integration, benchmarks, docs sweep

Deliverables: the combined vertical (`stress` under `sysmon` under QEMU:
pressure bands move, the pinned monitor's memory never enters `ramzip`,
both exit clean); benchmark evidence for the pinning default policy and
the query hot paths (paging cost bounded); README support-matrix rows
where per-arch state varies; `docs/src/` pages current; `PLAN.md`
STRESSTEST section moved to done-state summaries.

Stage order is binding: ST4 depends on ST1+ST2; ST5 on ST1+ST2+ST3+ST4
(for `--monitor`); ST6 on all. The ST5/ST6 assertions that `RAMZIP_STATS`
counters move additionally depend on the restartable-user-page-fault
prerequisite (§0); until it lands those verticals assert the pressure and
reclaim movement only, and the plan is updated when the tier switches on.

---

## 9. Invariants (binding)

- **No second data channel.** Every figure both apps show travels through
  the System Information API; no kernel back-door, no debug syscall, no
  log scraping (§16.6).
- **Counters, never contents.** No query, selector, or panel ever exposes
  page contents, key material, capability tokens, or another user's data
  beyond what its capability gate already licenses (§19.4, §23.1).
- **Fail closed, degrade gracefully.** Every refusal (capability, limit,
  ENOSPC) is a typed answer rendered/reported while the session continues
  where the action was incidental, and a stated fatal reason where it was
  the purpose (§2.24, §5.4).
- **Event-driven always.** Neither app ever spins: refresh, timeout,
  child exit, and signal are all waitset members. The *only* intentional
  tight loops are the stress workers' load units — that is their entire
  purpose, they run only for the requested duration, and they are
  ordinary preemptible user tasks (§2.23's carve-out does not even need
  stretching: generating load is the work, not a wait).
- **Pinning is bounded and observable.** `CAP_MEM_PIN` + the pinned-bytes
  rlimit + the `stats:mem/pinned` gauge land together; an unbounded or
  invisible pin is a defect (§24.3, §24.1).
- **Signals stay honest.** `Kill` is never observable or maskable; an
  opted-in process that stops draining is still terminable; the un-opted
  default is byte-identical to SP9.
- **The apps are ordinary bundles.** Full self-contained `.app` bundles
  discovered from the store, help in `Help/` only, coreutils-style
  options where a counterpart exists (§16.5, §16.7), fd 0/1/2/3 only
  (§20).
- **64-bit everywhere.** Byte counts `u64`, times `Time64`/`Duration64`
  (§21, §26.6).

---

## 10. Required test matrix summary

```text
observability:
  each new payload round-trips and rejects malformed/reserved bytes
  fuzz targets for every new decoder
  capability denial fails closed and is audited
  counters move when the underlying gauge/ledger moves
  resolver: new selectors resolve, unknown leaves fail closed,
    denial maps to CapabilityDenied, decorations unserviceable

pinning:
  pinned pages never selected for ramzip under pressure
  unpin restores eligibility
  pinned-bytes limit crossing fails closed (typed, no panic)
  CAP_MEM_PIN denial fails closed; audit event emitted
  not inherited across spawn; cleared on exit
  stats:mem/pinned reflects the aggregate

signal observation:
  opted-in Interrupt observed, not fatal; exit 130 after teardown
  Kill unconditionally fatal regardless of opt-in
  second pending Interrupt escalates to terminate
  un-opted process behaviour unchanged from SP9
  console ^C reaches the intake end-to-end (QEMU)

sysmon:
  model: keys, panel focus, refresh interval bounds, degradation states
  gated-query refusal renders as refusal, session continues
  pin refusal continues unpinned with notice
  alternate screen restored on quit (shell screen intact)
  idle: no work between deadlines

stress:
  option parsing: valid, invalid, contradictory (--monitor + --background)
  each worker kind produces its load and honours its byte target
  overcommit >100% yields typed refusals counted, never a crash/spin
  timeout expiry: workers terminated, reaped, scratch removed, exit 0
  ^C / Terminate: same teardown, exit 130/143
  worker killed externally: controller reports and continues teardown
  --quiet silences stdout summary, never stderr errors
  --background returns the prompt; controller keeps running
  scratch hygiene on every exit path

integration:
  stress under sysmon: pressure band moves, ramzip counters move,
    pinned monitor memory never compressed, both exit clean
```

---

## 11. Acceptance checklist

This work is complete only when all items are true:

- `AGENTS.md` read and obeyed; this plan and `PLAN.md` kept current in
  the same changes (§13).
- Rust-only; no C/C++/new assembly; generated headers regenerated where
  the ABI changed and drift gates green (§9).
- Every new query/syscall/capability/limit landed with a live caller,
  its enforcement point, docs, fuzz coverage, and audit events in the
  same change (§5.2, §16.6, §19.6).
- No new capability beyond `CAP_MEM_PIN`; no widening of `lib/resref`'s
  namespace registry.
- Both apps are complete self-contained bundles with `Help/` trees; no
  embedded help, no compiled-in app lists (§16.5).
- All §9 invariants hold; all §10 matrix rows have passing tests; no
  production `unwrap()`/`expect()`/`panic!()`/`todo!()`, no ignored
  tests, no retry loops.
- The whole-project gate has been run in the foreground to completion
  for every stage: `cargo fmt --all` (+ `--check`), `cargo xtask ci`
  exactly once, `cargo xtask fuzz --secs 5`, and the developer-capped
  soak (`tools/ci/soak.sh both --secs 20`); actual output quoted in the
  completion report with the §23.5 verdict.

---

## 12. Prompt for an implementation agent

```text
You are implementing the next approved stage of `plans/STRESSTEST.md` for
RustOS.

Before coding, read `AGENTS.md`, `PLAN.md`, `plans/STRESSTEST.md`,
`plans/SWAPSWAPSWAP.md`, `plans/SMARTRAM.md`, `plans/SPAWN.md`,
`plans/APPS.md`, `plans/CURSES.md`, `plans/ALIAS.md`,
`lib/abi/src/sysinfo.rs`, `lib/procinfo`, `userland/system/sysinfod`,
`userland/apps/top`, `kernel/mem` (pressure, reclaim, ramzip), and the
signal/wait/waitset/rlimit syscall surface.

State the assumptions you verified from the repository: the current
sysinfo query set and dispatcher seam, the pressure/reclaim/ramzip
counter locations, the Signal set and delivery paths, the waitset
timeout semantics, the rlimit LimitKind set, and the command-app bundle
conventions. Where this plan and the tree disagree, surface the mismatch
and stop.

Implement only the approved stage, completely: ABI + kernel + userland +
rustdoc + mdBook + unit/fuzz/property tests + QEMU verticals where
staged, with no stubs, no deferred tests, no speculative surface, and no
security defence weakened. Counters only — never page contents or
secrets. Fail closed; degrade gracefully where the plan says an action
is incidental.

Finish by running the full workspace gate in the foreground and waiting
for it to exit: `cargo fmt --all`, `cargo fmt --all --check`,
`cargo xtask ci` (exactly once), `cargo xtask fuzz --secs 5`, and
`tools/ci/soak.sh both --secs 20` on a developer machine. Quote actual
command output and state the AGENTS.md §23.5 verdict.
```
