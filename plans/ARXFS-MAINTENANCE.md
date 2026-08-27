# ARXFS-MAINTENANCE.md — autonomous filesystem health: the maintenance runner

Status: **planned** (stages M0–M7, none landed).
Binding under `AGENTS.md` and listed in its §15.18 jump-sheet.
This plan owns ARXFS spec **stage 18** (`docs/src/filesystem/arxfs-spec.md`
§15/§18), whose spec section is **§24**.
Primary code area: `drivers/filesystem/arxfs/`, `kernel/tairix-kernel/`,
`lib/abi/src/driver/{block,filesystem}.rs`, `lib/abi/src/blkio.rs`,
`userland/apps/`.

**Dependency.** The runner writes to the volume (trim, copy-repair, the health
baseline, the scrub cursor) on a cadence measured in hours, so it lands *after*
the write-back cache and the commit barrier (`plans/ARXFS-WRITEBACK.md`, spec
stage 17): a background writer on today's barrier-less commit path would
multiply the exposure D63 describes (`plans/OPEN-DEFECTS.md`) across every
maintenance pass, and would pay a device-cache flush per discarded run.

---

## 1. The defect: implemented, undriven

ARXFS ships `scrub`, `check`, `trim`, `health`, and `rescue` — each specified
(spec §11, §12), implemented, capability-gated on `CAP_FS_MOUNT`, and
unit-tested. **None has a production caller.** Nothing in the kernel, a
service, or a command app invokes any of them, and there is no `arxfs` command
app, so no administrator can reach them either.

What that costs on a live system:

- **TRIM never issues.** `discard.rs` accumulates freed blocks in a transient
  in-memory pending-discard queue as transactions reclaim them; the queue is
  never drained and is dropped at unmount. Every flash device the system ever
  writes therefore believes its entire written history is live: write
  amplification and wear climb monotonically, and the device's own garbage
  collector works against a full-looking drive forever. The batching, the
  granularity alignment, and the per-call rate limit already in `trim` exist
  *because* it was designed to be driven incrementally by something that was
  never written. Worse, driving that queue would not have been enough: it
  silently and permanently loses eligibility in three places (§13 D-M3), so the
  fix replaces it with a sweep of the allocation map before the runner exists.
- **Latent media errors are found by the first read that needs the data.**
  Scrub's whole purpose is to find a decaying block while a good copy still
  exists — metadata has a mirror, and stage 21 will add parity. Undriven, the
  first observer of bit rot is a user read, at which point the redundancy that
  could have healed it may itself have decayed.
- **The health baseline never advances past mkfs.** `health` compares device
  telemetry against a persisted baseline and is the thing that schedules a
  scrub on an unsafe-shutdown or media-error delta. With no caller the baseline
  stays at its format-time value, so the deltas grow without bound and mean
  nothing, and the mount-time comparison spec §11 requires never happens.
- **`scrub`'s resumable cursor is dead weight.** It persists a cursor and
  guarantees a resumed pass is identical to an uninterrupted one — a property
  only a scheduler that pauses and resumes can use.

Two adjacent plans already presume a driver that does not exist:
`plans/SPARSE.md` §8/§11 refer to "existing background optimisation" and to
scrub reporting a sparse-conversion opportunity, and `plans/ARXFS-SNAPSHOT.md`
§3.3 requires the post-deletion free/discard pipeline to be "incremental and
interruptible" — which is a statement about a scheduler, not about snapshots.

## 2. What "self-maintaining" means here

The filesystem keeps itself healthy **without a human and without a
configuration file**: it discards what it has freed, verifies what it holds
while it can still repair it, watches the device telemetry, and escalates what
it cannot fix itself. It does that as a *paced background consumer of its own
volume*, subordinate to the foreground workload at every moment.

What it must never mean:

- **Never a periodic tick.** No fixed-frequency timer, no `HZ`, no polling
  loop. The runner parks and is woken by an event or a single armed one-shot
  deadline; a machine with nothing to maintain takes no interrupts for it.
- **Never a busy-poll or a yield loop.** `loop { maybe_work(); yield_now() }`
  is the anti-pattern the charter names; the runner blocks on a wait queue.
- **Never ambient authority.** The runner does exactly what the mount it serves
  could already do, under that mount's own authority, and can reach no other
  volume (§9).
- **Never an unbounded call.** Every action is one bounded chunk with a
  persisted resume point. A 100 TB volume is maintained in chunks that fit a
  1 GiB machine, never in one call and never with a whole-volume structure
  resident (§12).
- **Never a foreground stall.** The runner yields to real work; it holds the
  mount lock for one chunk, not one pass.
- **Never a repair that guesses.** Scrub repairs a bad copy from a good one and
  reconciles derived state toward extent-derived truth; anything it cannot
  prove it reports and escalates. Automatic `check` on a mounted volume is
  impossible by construction and automatic *aggressive* repair is forbidden
  (`plans/SPARSE.md` §13 names the unsafe repairs).
- **Never a knob.** One production profile (spec §1). Cadences derive from the
  discovered device class and the live pressure gauge, never from a mount
  option, an environment variable, or a settings file.

## 3. Invariants

- **One scheduler, one pacer.** The decision "what should this volume do next,
  and when" lives in exactly one place per layer, and the *pacing arithmetic*
  every layer shares lives in exactly one place full stop (§5).
- **Pure and host-provable.** The scheduler holds no clock, spawns no timer,
  performs no I/O, and allocates nothing: the caller supplies the monotonic
  reading it took and the observations it made, and gets back an action and an
  absolute deadline. Every policy decision is therefore a host unit test, not a
  QEMU timing observation.
- **Restoring redundancy outranks verifying it, and both yield to the
  foreground.** This holds *across* layers, not only within one (§6).
- **Progress survives a restart.** A pass measured in hours resumes where it
  stopped. The chunk size is derived so that one cursor persist per chunk lands
  at the checkpoint cadence — the metadata write is amortised, never one per
  block and never absent.
- **Read-only means read-only.** A read-only mount runs the *verifying* actions
  and reports; it writes nothing — no copy-repair, no baseline, no cursor, no
  discard (§13).
- **Every action is audited.** Start, outcome, escalation, and refusal each emit
  a stable event ID on the hash-chained log, naming the volume. A background
  writer that leaves no record is unauditable by construction.
- **Fails closed, never panics.** A faulting chunk stops that volume's pass,
  records the fault, escalates the volume's state, and backs off on the shared
  cadence — never retries in a loop, never publishes a partial transaction.
- **Bounded per machine, not per volume.** One runner serves every mounted
  volume. Several 100 TB volumes on a 1 GiB machine cost one task and one
  bounded chunk buffer, not a task and a slab each.
- **No new capability, no new mount state, no second data path.** The runner
  calls the operations that already exist, through the driver ABI that already
  serves the mount.

## 4. The scheduler

One new module, `drivers/filesystem/arxfs/src/maintain.rs`, holding
`VolumeMaintenance`: the pure, event-timed decision for one mounted volume.
Its shape follows the one already proven for composed arrays
(`lib/raid/src/maintenance.rs` — `next_action` / `note_step` /
`note_foreground` / `wait_deadline_ns`), because a serve loop that must decide
what background work to do next, pace it against the foreground, and park on a
deadline is the same problem at a different layer.

### 4.1 Actions

```text
MaintenanceAction:
    Trim                     sweep one bounded span of the free map, discarding runs
    Health                   read telemetry, compare to baseline, persist the new one
    Scrub { budget }         verify one bounded chunk, persisting the cursor on pause
    Idle                     nothing to do; park until wait_deadline_ns
```

There is no `Checkpoint` action: `scrub` already persists its cursor when its
budget pauses it, so the chunk *is* the checkpoint. There is no `Check` or
`Rescue` action: both are offline supersets that rebuild derived state, and
neither is a thing a mounted volume may do to itself (§10).

`Health` is not merely a telemetry read: it is what *decides* a scrub is due.
The existing `health` already computes `metadata_scrub_recommended` and
`deep_scrub_recommended` from the baseline deltas — the scheduler consumes
those recommendations rather than re-deriving them.

### 4.2 Priority

1. **`Trim`** whenever the discard sweep has free space it has not reached —
   the cursor is mid-pass, or space was freed behind it and the newest such
   free has aged past the device class's dirty-age window (the same class-keyed
   window the commit scheduler derives, `plans/ARXFS-WRITEBACK.md` §5, not a
   second constant). Discard is cheap, bounded, and the only action whose
   *deferral* costs the device endurance rather than merely delaying
   information.
2. **`Health`** when the interval has elapsed, at mount, and after any
   escalation. It is one telemetry read plus one small transaction, and it
   gates whether a scrub is due at all.
3. **`Scrub`** when health recommends one (an unsafe-shutdown or media-error
   delta), when a pass is already in flight, or when the exposure window since
   the last completed pass has elapsed. A scrub in flight is *paused*, not
   abandoned, when a higher-priority action or a stand-down (§6) intervenes.
4. **`Idle`** otherwise, with `wait_deadline_ns` naming the soonest of: the
   discard age deadline, the health interval, the scrub exposure deadline, and
   the pace hold-off from the last chunk.

`Trim` above `Scrub` deserves its reason stated: a scrub is a *detector* whose
value decays slowly, while free space the device still believes is live
actively degrades the medium the whole filesystem sits on, and sweeping it is
bounded work.

### 4.3 Triggers, not timers

The deadline is the *fallback*. The runner is woken by the events that change
the answer, so it acts promptly without polling:

| Event | Why it changes the decision |
|---|---|
| a transaction commits with freed blocks | free space appeared the sweep has not reached |
| a snapshot is deleted (stage 20) | a potentially large free set appeared at once |
| the backing device reports a health transition | telemetry moved; a scrub may now be due |
| the backing device returns to `Available` (§6) | a stood-down pass may resume |
| a foreground operation completes | the pace hold-off may now be satisfiable |
| memory pressure changes band | the chunk size and cadence change |
| unmount / detach / power-down | the pass must stop cleanly at a chunk boundary |

Mount is itself a trigger: an unclean previous shutdown is exactly the
condition spec §11 says schedules a scrub, and it is known at mount.

### 4.4 State

Per volume, in memory beside the mount: the last-chunk duration and completion
time (for the pace), the last health read, the last completed-pass time, the
in-flight-pass flag, the escalation state, the bounded recently-freed hint
(§13 D-M3), and the shared retry record. The *durable* state is the health
baseline and the two resumable-pass cursors — scrub's, which exists, and the
discard sweep's, which replaces the lost-eligibility queue. Both cursors are
rebuildable: losing one costs a re-sweep, never correctness, so a runner that
never ran leaves nothing a mount depends on.

## 5. Shared pacing: one pacer, one class-keyed budget

`lib/raid/src/maintenance.rs` already answers four questions that are not
RAID's: how long after a foreground request a storage consumer still counts as
busy; what share of wall-clock a background chunk may take on a device of a
given class; how often an advancing pass persists its position; and the
escalating cadence a failed attempt is held off by. Those answers are equal by
definition for a filesystem scrub and an array scrub — they are properties of
the workload's burstiness, the device class, the re-work an operator tolerates,
and the accepted exposure window — so a second copy in ARXFS would be the
duplication the charter forbids, and worse: two layers pacing to two different
notions of "busy" cannot compose (§6).

The work is therefore to **hoist, then consume**:

- The class-keyed background budget (`busy_duty_percent`, `foreground_idle_ns`,
  `checkpoint_period_ns`, and the exposure window) joins `IoBudget` in
  `lib/abi/src/blkio.rs`, which is already the one home for per-`BlkDeviceClass`
  storage policy and its small pure state machines (`IoBudget::should_reissue`,
  `BlkHealth::poll`, `recovery_wait_timeout`). Both consumers already depend on
  `lib/abi`, so this adds no crate and no dependency edge.
- The duty arithmetic — *after a chunk taking `d`, hold the next off for
  `d × (100 − duty) / duty`; an idle consumer runs at full speed* — becomes one
  pure `DutyPacer` value type beside it, consumed by both schedulers.
- `lib/raid`'s `MaintenancePolicy` keeps its RAID-specific fields (the member
  re-add cadence) and derives the shared ones from the hoisted budget instead
  of holding its own copies. Its behaviour must not change: the existing RAID
  maintenance tests are the regression proof for the hoist.

What is **not** shared is the action vocabulary and the priority order: an
array re-admits members and rebuilds parity, a filesystem discards and
verifies. One pacer, one budget table, two action sets.

## 6. Stacked layers: the cross-layer stand-down

ARXFS may sit on a composed array (`drivers/storage/raid`), which sits on
physical devices. Both layers want to run background verification over the same
spindles. Two independent schedulers, each politely taking its 10 % duty share,
take 20 % of a device that the foreground workload is trying to use — and worse,
a filesystem scrub can spend the exact bandwidth a degraded array needs to
restore its redundancy, which inverts the priority the charter and
`lib/raid/src/maintenance.rs` both state.

The rule, stated once: **restoring redundancy below outranks verifying above.**

- ARXFS's *background* actions stand down entirely while the backing reports
  anything other than fully available. A degraded or rebuilding array's
  bandwidth belongs to its rebuild; a device inside its recovery grace window
  is being given a bounded chance to come back and must not be handed
  discretionary reads. An in-flight scrub pauses at its cursor — it is not
  abandoned, and it is not pressed on without a copy to repair from.
- ARXFS's *foreground* work is unaffected: this governs discretionary
  background I/O only, never a user's read or write.
- `Trim` stands down too, and for a stronger reason than pacing: a discard is
  destructive and irreversible, and issuing one to a fault domain whose state
  is in doubt is exactly the "no data-loss shortcut" the charter forbids.

To ask the question, the block seam gains one default-provided query:

```text
Block::backing_availability(&self) -> MountAvailability     default: Available
```

`MountAvailability` is reused rather than invented: it is already the shared
vocabulary for *available / degraded / recovering / unavailable*, and
`lib/abi/src/raid.rs` already maps array health into it. The default is the
honest answer for a plain device that has nothing composed beneath it. Two live
producers land with the query — the RAID composer's `Block` impl (from its
array health) and the kernel block client (from the per-device `BlkHealth`
state machine and the volume service's unavailable states) — and one live
consumer, this scheduler, so nothing speculative is added. No new capability:
it reports a state the caller's own device already has.

`arxfs`'s reported volume health takes the worse of its own findings and the
backing's availability, so `arxfs status` on a filesystem over a degraded array
says so instead of claiming a clean bill on sand.

## 7. The production driver: the maintenance runner

The scheduler decides; something must drive it. The driver is **one kernel
maintenance task** — `kernel/tairix-kernel/src/fs_maintain.rs` — admitted
through `InitSpawnCtx::spawn_kernel_service`, exactly as the driver-store server
and the root-unlock service are.

Per turn, for the volume whose deadline is soonest:

1. take the mount's sleeping lock (the same lock the `fs_*` syscalls take, so a
   chunk can never interleave with an operation);
2. ask that volume's scheduler for its next action, having handed it the
   monotonic reading, the backing availability, and the pressure band;
3. perform one bounded chunk through the driver ABI (§8), releasing the lock;
4. hand the outcome and its duration back to the scheduler;
5. register on the maintenance wait queue with the soonest deadline across every
   mounted volume, and **park**.

That is the whole loop. It is event-driven and tickless: registration carries a
finite deadline only when some volume actually has pending work, so an idle
machine arms nothing, and every trigger in §4.3 is a wake on that queue rather
than a poll. It never holds the mount lock across a park.

**One task, not one per volume.** A task per mount is the fixed per-volume cost
the scalability rule forbids — several 100 TB volumes on a 1 GiB machine would
each take a kernel stack and a chunk buffer. One task with a per-volume
scheduler record and one shared chunk buffer is bounded by the machine, and its
round-robin over volumes with elapsed deadlines is what keeps one busy volume
from starving another.

**Where it lives, and where it will live.** ARXFS is hosted in-kernel today
because the root filesystem is the bootstrap floor — the store of drivers and
services cannot be read before the volume holding it is mounted. The runner
therefore sits beside the mount it serves. It is written against the driver ABI
(§8) and holds no ARXFS type, so when ARXFS moves out into a user-space
filesystem driver process the same scheduler runs in that process's serve loop
against the same seam, and the kernel copy goes away with the in-kernel host.
The scheduler being pure and ABI-typed is what makes that a move rather than a
rewrite.

**Pressure and the chunk.** The chunk buffer is charged to the reclaim ledger
and its size is derived from discovered RAM, floored at one block. A rising
pressure band shrinks the chunk and lengthens the cadence; the critical band
stands the runner down entirely, because discretionary verification is the
first thing a machine under memory pressure should stop doing.

## 8. The driver ABI seam

The runner holds `Box<dyn KernelFs>`, not an `ARXFS<B>`, so the maintenance
surface must be part of the filesystem driver trait set — there is no other
seam, and a downcast to a concrete driver type would be the hack the charter
forbids.

`lib/abi/src/driver/filesystem.rs` gains a `FilesystemMaintenance` facet
alongside `FilesystemAttrs` / `FilesystemStats`, and `KernelFs` gains it as a
bound:

```text
FilesystemMaintenance:
    maintenance_support() -> MaintenanceSupport   what this driver can do at all
    discard_backlog()     -> DiscardBacklog       unswept free space + age of the newest free
    trim_chunk(caps, sink, budget)                one bounded sweep of the free map
    health_pass(caps, sink)                       telemetry + baseline + recommendations
    scrub_chunk(caps, sink, budget)               one bounded verify chunk
```

Both chunk calls take a budget and report whether the pass completed or paused,
so the runner paces them by the same rule and neither can run away.

Every method is default-provided and honestly refuses: a driver that cannot
maintain itself reports `MaintenanceSupport::none()` and the scheduler never
asks it anything, so ext4, FAT32, and the in-memory filesystem need no change
and the runner needs no per-driver special case. `abi-v1` is unfrozen, so the
facet is added in place — no `v2`, no shim.

The wrapper layers forward it like every other facet (`CachedFs`, the
group-mapping wrapper, `Box<dyn KernelFs>`), and the existing wrapper
conformance suite (`kernel/core/src/fs/wrapper_conformance.rs`) is extended so a
wrapper that swallows a maintenance call fails a test rather than silently
disabling maintenance for every mount that happens to be wrapped.

## 9. Authority and audit

The runner needs `CAP_FS_MOUNT` — the capability every one of these operations
already checks. It does not get it ambiently and it does not get a new one.

- **No new capability.** `CAP_FS_MOUNT` already expresses "may operate on this
  volume's whole-volume state", it is what the existing entry points enforce,
  and the only conceivable holder of a `CAP_FS_MAINTAIN` would be the mount
  owner. Under the capability-minimalism test it fails on all three counts, so
  it is not added.
- **Authority is per mount and derived, not machine-wide.** The runner's
  capability set for a volume is derived from the authority that established
  that mount (the `volume_attach` caller's verified grant, or the boot path's
  for the root volume) and is attached to that volume's scheduler record. A
  runner cannot act on a volume whose mount authority it does not hold, so a
  volume attached by a less-privileged principal is not maintained by borrowing
  another mount's rights.
- **Every chunk is audited** with a stable event ID naming the volume, the
  action, the outcome, and — for a refusal — the reason. The audit answers the
  question a background writer must always be able to answer: *what wrote to
  this device, when, and on whose authority.*
- **Escalations are security events**, not log lines: an unrepairable finding, a
  both-copies-bad metadata block, a device crossing a critical threshold, and a
  read-only downgrade each emit their own event and are visible through the
  System Information API.

## 10. What stays operator-driven

`check`, `rescue`, and `grow` are **not** background actions and never become
them. `check` and `rescue` are offline: `check` rebuilds derived state and needs
the volume not serving, `rescue` reads a volume too damaged to mount. `grow`
changes the volume's geometry on an operator's instruction. Automating any of
them would be automating a decision that is not the filesystem's to make.

They are reachable, and the health the runner accumulates is readable, through
one new command app — a self-contained bundle under the system command store,
discovered from disk like any other (there is no `arxfs` command app today, so
spec §12's operations are currently unreachable by an administrator):

```text
arxfs status   [<volume>]     health state, last scrub, unswept free space, backing availability
arxfs scrub    <volume>       run or resume a pass now, foreground, progress on stdout
arxfs trim     <volume>       run the discard sweep to completion now
arxfs health   <volume>       telemetry, baseline deltas, thresholds crossed
arxfs check    <volume>       offline structural validation and repair
arxfs rescue   <volume> <out> extract from a volume too damaged to mount
```

Every subcommand goes through the same capability-checked ABI the runner uses —
never a privileged bypass — binds to the standard streams only, follows the
coreutils option and diagnostic conventions, and emits the structured advisory
records on `stdinfo` (an omission record when a listing is partial, a summary
record for a pass's findings) without altering stdout. Its help is authored in
the bundle's own `Help/` tree; nothing is compiled into the binary.

**The escalation that survives a reboot.** When scrub finds structural damage
it cannot repair, the volume needs `check`, which the mounted volume cannot run
on itself. The runner therefore sets a sticky *check-requested* mark in the
volume's own metadata and escalates: the mark is reported by `arxfs status` and
the mount snapshot, and it drives the mount's availability state so a tool shows
the volume as at-risk. The mark is cleared only by a `check` that completes.
It does **not** silently run a multi-hour `check` on the next boot of a 100 TB
volume, and it does not refuse the mount: the volume keeps serving what it can
verify while telling the truth about what it cannot. The pre-boot Supervisor is
the surface from which an operator acts on the mark before a volume is mounted
(`plans/NEW-SUPERVISOR.md`).

## 11. Reporting

- **System Information API.** The mount record already carries availability; it
  gains the volume's maintenance state — last completed scrub, pass progress,
  findings since the baseline, unswept free space, check-requested mark,
  backing availability. Never a `/proc`-style view.
- **The audit log** carries every action and escalation (§9).
- **`stdinfo`** carries the advisory summary from the command app (§10).
- **The desktop** consumes the sysinfo query it already reads for mounts; no
  GUI-specific path and no new endpoint.

## 12. Prerequisites this plan cannot proceed without

These are not maintenance work, but a paced background pass is impossible until
they land, so they are named here and sequenced ahead of M2 in
`plans/IMPLEMENT-OUTSTANDING-ARXFS.md`.

- **Bounded tree iteration — done** (item A0 of
  `plans/IMPLEMENT-OUTSTANDING-ARXFS.md` §3). `TreeWalk` is the driver's only
  multi-record read: one step, one root-to-leaf path, one leaf's records from
  the walk's own block-sized buffer, positioned by a single key so a pass may
  stop, persist it, and resume. The scrub and check walks are converted and the
  collecting forms are gone. What is *not* yet bounded is the state those passes
  accumulate around the walk (§13 D-M5), which M2 needs first.
- **A work-shaped scrub budget.** `ScrubBudget::Inodes(n)` bounds *inodes*, and
  `scrub_inode` has no budget check inside it, so a single 100 TB file is one
  "unit" and `Inodes(1)` is an uninterruptible multi-hour call holding the mount
  lock. The budget must count verified blocks, and the cursor must be
  `(inode, logical offset)` so a pass resumes *inside* a large file. This is the
  same requirement the charter puts on every long-running whole-volume
  operation: bounded forward progress, cancellable, progress reported.

## 13. Defects this plan owns

Found by reading the code while designing the runner. Each is fixed with its
regression test in the stage named, not deferred.

**This list is open, and every entry on it gets fixed.** A defect found while
implementing any stage of this plan is governed by the no-deferral rule
(`plans/IMPLEMENT-OUTSTANDING-ARXFS.md` §6): fixed in that stage with a
regression test, or — when it is genuinely too large for it — written up here
and made the **immediate next stage**, ahead of every M-stage below it. Size is
not an exit and a write-up is not a fix; an entry here says the next piece of
work is closing it. So this plan is not finished when M7 lands: it is finished
when M7 has landed **and** this section is empty.

- **D-M1 — `scrub`'s copy-repair write bypasses the read-only guard**
  (`plans/OPEN-DEFECTS.md` D64, tracked there for its severity).
  `read_meta` correctly guards its repair-on-read write with `if
  !self.read_only`; `scrub_meta_into` performs the same repair with
  `self.write_block(comp, …)` and **no** guard, and neither `scrub` nor `health`
  calls `deny_if_read_only` (only `trim` does). So a read-only mount writes to
  its device. That is wrong wherever `read_only` is set, and it is dangerous in
  the one case the state exists for: a re-inserted volume whose non-mutation
  could not be proven is mounted read-only *with its uncommitted write set
  held* precisely so nothing touches a medium whose state is in doubt — and a
  copy-repair there mutates it. `health` is additionally wasteful on a
  read-only handle: it performs the whole telemetry read and (today) a
  whole-volume scrub, then fails at the baseline `commit()`, discarding a valid
  health reading it could have reported. Fix in **M1**: one read-only rule
  shared by both repair sites, `scrub` and `health` verifying and reporting
  without writing on a read-only handle, and a test that a read-only mount
  issues no device write during a scrub that finds a repairable block.
- **D-M2 — `health` runs an unbounded whole-volume scrub inline.** On a
  baseline delta, `health` calls `self.scrub(caps, sink, ScrubBudget::Unlimited)`
  in its own call, holding the mount lock for the whole volume's verification —
  hours on a large volume, with every VFS operation blocked behind it, and the
  allocation of §12 on top. `health` must *recommend*; the scheduler runs the
  pass in paced chunks. Fix in **M2** (the recommendation is already computed;
  the inline call is deleted).
- **D-M3 — discard eligibility is silently and permanently lost, so TRIM can
  never be made correct by driving it.** The pending-discard queue is a
  `Vec<u64>` of individual block numbers, capped at a fixed
  `MAX_PENDING_DISCARD` of 65536 entries, and `enqueue_discard` **drops** a
  block once the cap is reached. Its rustdoc says a dropped entry "stays
  un-discarded (still free) until a future free, trim pass, or mount rebuild
  requeues it" — and no such requeue exists. Nothing walks the free map to find
  never-discarded free blocks; the mount-time free-space rebuild rebuilds the
  *allocation map*, not the queue, and only runs when the map cannot be adopted.
  So a block freed once, dropped, and never reallocated is never discarded again
  for the life of the volume. There are three loss paths:
  1. **The cap.** One operation that frees more than 65536 blocks — deleting or
     truncating a file larger than 256 MiB on a 4 KiB volume, 32 MiB on a
     512-byte one — silently loses the excess at enqueue time.
  2. **A fault mid-pass.** `trim` empties the queue with `mem::take` before it
     starts, then returns on the first `discard` error, dropping every
     not-yet-processed run. One transient device fault permanently forfeits a
     whole pass's eligibility.
  3. **The deferred remainder.** `requeue_range` re-enqueues block by block
     through the same capped path, so a batch-limited or granularity-trimmed
     remainder is subject to loss path 1 again.
  A perfect runner over this queue would therefore *look* like it was working
  while the device never learned about most of the freed space — the worst
  possible failure mode for a plan whose point is that TRIM issues.
  **Fix in M2**, before the runner exists, and structurally: delete the
  per-block queue and sweep the **allocation map**, which is already the
  authoritative record of what is free. A persisted discard cursor advances
  through the map in bounded chunks, coalescing runs of free blocks and
  discarding them, exactly as the scrub cursor advances through the inode tree —
  same resumable-pass shape, same reserved-owner progress block, O(1) memory,
  and eligibility that cannot be lost because it is derived from the map rather
  than remembered beside it. A small bounded in-memory hint of recently-freed
  runs lets the sweep visit them first; losing the hint costs latency only,
  never eligibility. This also deletes trim's `sort_unstable`, `dedup`, and
  runs vector, each of which is sized by the queue it no longer has. The cursor
  is rebuildable state like the scrub cursor, so a lost cursor costs one
  re-sweep and never correctness. If the sweep cannot be given a home without
  changing the transaction-root layout, **stop and ask** rather than guessing an
  on-disk change.
- **D-M5 — `scrub` and `check` hold whole-volume state in RAM, so neither can
  complete on the machine the charter requires the volume to be served from.**
  Found while converting these passes to the bounded walk (item A0): the
  *iteration* is bounded now, the accumulators around it are not.
  `scrub_refcounts` builds a `BTreeMap<u64, Vec<Referrer>>` keyed by **every
  physical data block on the volume** before it reconciles anything — tens of
  billions of entries on the combined floor, so the reconcile phase of both
  `scrub` and `check` is unreachable there, not merely slow.
  `check_directories_and_orphans` holds a reachable-inode `BTreeSet<u32>` and
  its walk stack, `check_link_counts` a `BTreeMap<u32, u32>` of every named
  inode, and `check::dir_entries` a `Vec` of a whole directory's entries. The
  fix is not more iteration: the derived truth needs a bounded home. ARXFS
  already has the shape — an on-disk paged structure with a bounded resident
  cache — so the reconcile streams the extents through a scratch map (a second
  claim on a block is the double-mark the map already detects) and verifies each
  chunk with bounded lookups against the chunk and reverse-reference trees,
  behind a persisted cursor like the scrub cursor. That is an on-disk layout
  decision: **stop and ask before guessing one.** Fix in **A2**, which is
  sequenced ahead of M2 in `plans/IMPLEMENT-OUTSTANDING-ARXFS.md` because a
  "bounded pass" over an unbounded accumulator is not one.
- **D-M4 — `health` commits a whole transaction on every poll even when the
  baseline has not changed.** `health` unconditionally runs `begin()` /
  `store_health_baseline` / `commit()`. On a device reporting
  `DeviceHealth::Unavailable` with no new fault counters — every virtio device,
  and any card without health passthrough — the new baseline is byte-identical
  to the stored one, and writing it costs the baseline block and its mirror, a
  fresh transaction root and its mirror, and a superblock slot and its mirror:
  six device writes and three HMACs to store what was already there. At the
  runner's cadence that is tens of thousands of pointless writes per volume per
  year, on precisely the devices that gain nothing from them. Fix in **M2**:
  compare the derived baseline with the stored one and skip the transaction
  when they are equal — the poll still reports its reading.

## 14. Interaction with the other ARXFS stages

- **Write-back (stage 17).** Trim, scrub, health, grow, check, and rescue are
  already on that plan's barrier-requiring list: each closes the open
  transaction before it runs, because discard eligibility and verification read
  *committed* state. The runner is a client of that rule, not a second copy of
  it. Its own writes (the baseline, the cursor) are ordinary transactions.
- **Snapshots (stage 20).** Deleting a snapshot frees whatever was reachable
  only from it, which wakes the runner (§4.3) and is then swept like any other
  free space — the runner is the "incremental and interruptible" freeing that
  plan requires. The sweep makes this *easier*, not harder: it discards what the
  allocation map says is free, so it needs no per-block record of a
  potentially enormous deletion and cannot lose part of it. Whether a block is
  free stays the reachability authority's answer, in one place, extended to the
  snapshot root set by that plan and not restated here.
- **FEC (stage 21).** FEC's persistent job engine (FEC13) schedules rebuild,
  rebalance, and its own scrub. It is the same shape as this scheduler and
  **must consume the same pacer and the same class-keyed budget** (§5) rather
  than adding a third; its foreground-priority requirement and this plan's are
  the same requirement. Where FEC gives ARXFS its own redundancy, the
  cross-layer stand-down (§6) becomes internal to ARXFS for those devices —
  restoring parity outranks verifying it, decided by one priority order.
- **Sparse.** Scrub already decrypts and hashes every data block it verifies,
  so noticing that a block's plaintext is all zero is free at that point; the
  runner reports the sparse-conversion *opportunity* in the scrub findings.
  Performing the conversion is a data rewrite with refcount and snapshot
  interactions and is **not** a health action (§17). `plans/SPARSE.md` §8/§11
  are corrected to say so rather than to imply a background optimiser exists.
- **Metadata.** No interaction: attribute blocks are ordinary mirrored metadata
  and are already covered by the scrub walk.
- **The §5 format targets (stage 19).** Independent, but they interact in the
  runner's favour: a wider filesystem block means fewer blocks per unit of free
  space and per unit of verified data, so both passes cover a volume in fewer
  chunks. Nothing here assumes the current geometry.

## 15. Tests

1. **Scheduler, host-pure**: priority order across every combination of pending
   conditions; `wait_deadline_ns` is the minimum of the live deadlines and
   `None` when nothing is pending; the duty pace holds the measured share for
   any chunk duration; an idle volume runs at full speed; a foreground report
   inside the busy window defers the next chunk; the escalation cadence backs
   off and never retries in a loop; round-robin across volumes cannot starve one.
2. **Pacer hoist is behaviour-preserving**: the existing `lib/raid` maintenance
   tests pass unchanged against the hoisted budget and pacer; one definition
   remains (a test asserts the RAID policy derives, not duplicates).
3. **Cross-layer stand-down**: a backing reporting `Degraded` / `Recovering` /
   any unavailable state yields `Idle` for every background action; an in-flight
   scrub pauses at its cursor and resumes at the same cursor on return to
   `Available`; foreground I/O is unaffected throughout; a trim is never issued
   to a non-available backing.
4. **Read-only**: a read-only mount performs no device write during a scrub that
   finds a repairable block, reports the finding, and returns a valid health
   reading (D-M1); a read-only mount never trims.
5. **Bounded chunks**: a scrub chunk over a single file larger than the chunk
   budget stops inside the file, persists a cursor, and resumes there; resident
   bytes during a pass over a large volume stay within the derived chunk bound;
   no whole-tree allocation occurs (asserted against an allocation counter).
6. **Resume equivalence**: a pass driven in N chunks produces the same findings
   as one uninterrupted pass, across an intervening unmount and remount.
7. **The discard sweep** — split by the stage that lands it.
   - **7a, the mechanism (M2, D-M3):** a reallocated block is never discarded;
     runs respect the granularity alignment; the three former loss paths each
     get a test that fails against the old queue and passes against the sweep —
     freeing far more than 65536 blocks in one operation eventually discards
     **all** of it, a device fault mid-pass forfeits nothing and the next pass
     covers the same space, and a granularity-trimmed or pace-deferred
     remainder is still discarded later. Resident bytes during a sweep over a
     volume with a large free set are independent of how much is free, and the
     cursor resumes a partial sweep exactly.
   - **7b, driven (M5):** a workload that frees blocks results in device
     discards with no explicit call, paced, and without the queue that no
     longer exists.
8. **Health** — split the same way.
   - **8a, the mechanism (M2, D-M2/D-M4):** an injected unsafe-shutdown delta
     *recommends* a scrub and runs none inline; an injected media-error delta
     recommends the deep pass; a poll whose derived baseline equals the stored
     one issues **no** device write, asserted on the write counter, and still
     returns its reading; a device without telemetry is recorded, not failed.
   - **8b, driven (M5):** the baseline advances past mkfs without an explicit
     call, a recommendation results in a paced scrub, and the runner still
     performs the actions that need no telemetry on a device that has none.
9. **Escalation**: an unrepairable finding sets the check-requested mark, emits
   its event, surfaces in the mount snapshot and `arxfs status`, survives a
   remount, and is cleared only by a completed `check`; a critical device
   threshold downgrades the mount to read-only with its own event.
10. **Event-driven, not polled**: with nothing to maintain, the runner arms no
    deadline and consumes no CPU over a long idle window (asserted on the
    task's accounted time and the armed-timer state); each §4.3 trigger wakes it
    without a timer having elapsed.
11. **Failure containment**: a faulting chunk stops that volume's pass, records
    the fault, backs off, and leaves other volumes' passes advancing; a faulting
    barrier publishes nothing; the runner never panics on any injected fault.
12. **Authority**: a runner record without the mount's authority is refused and
    audited; every action emits its event ID; no action succeeds on a volume
    whose mount authority the record does not hold.
13. **ABI seam**: a driver reporting no maintenance support is never asked for a
    chunk; every wrapper forwards the facet (wrapper conformance); ext4/FAT32/
    memfs mounts are unaffected.
14. **The combined floor**: small discovered RAM with several large volumes
    mounted, written, and maintained at once — bounded resident bytes, bounded
    task count, no panic, no busy-spin, fail-closed on exhaustion.
15. **Command app**: each subcommand's option surface and diagnostics follow the
    coreutils conventions; each is refused without the capability and audited;
    `stdinfo` records are emitted and never change stdout; help resolves from the
    bundle's `Help/` tree.
16. **Fuzz**: the scheduler's observation inputs (durations, deadlines,
    availability, backlog) as an untrusted-shaped harness asserting it always
    returns an action and never panics; the command app's argument parser.

## 16. Staging

One stage per session, each ending with the whole-project gate green and this
file's status plus spec §24 updated before it is reported.

### M0 — the shared pacer and the cross-layer query. **planned**
Hoist the class-keyed background budget and the duty pacer into
`lib/abi/src/blkio.rs`; make `lib/raid`'s policy derive from them with its tests
unchanged. Add `Block::backing_availability` with its two producers.
*Acceptance:* one definition of the pace and the budget in the tree; the RAID
maintenance suite passes unchanged; a composed array reports its degraded state
through the query and a plain device reports `Available`.

### M1 — the read-only rule (D-M1). **planned**
One shared read-only repair rule for both copy-repair sites; `scrub` and
`health` verify and report without writing on a read-only handle.
*Acceptance:* test 4.

### M2 — bounded passes: scrub, discard, and health (D-M2, D-M3, D-M4). **planned**
The stage that makes every maintenance operation a bounded, resumable, lossless
pass, so the runner has something honest to drive. Consumes the §12
prerequisites: the bounded walk (done) and the bounded reconcile state A2
lands. Three pieces: cursor-based iteration in the scrub walk with a
work-shaped budget and an `(inode, offset)` cursor; the discard sweep over the
allocation map replacing the per-block queue outright, with its own persisted
cursor and the bounded recently-freed hint; and `health` recommending instead of
scrubbing inline and skipping the transaction when its baseline is unchanged.
It also rewrites the two places the spec describes the queue — §11's
mounted-trim rules and §18's "Discard and health (10, 11)" summary — because it
changes that mechanism; every invariant they state (discard only when
unreachable, batched, granularity-aligned, rate-limited, a crash costs nothing)
still holds and is restated as a property of the sweep.
*Acceptance:* tests 5, 6, 7a, 8a; the inline `Unlimited` call, the
`MAX_PENDING_DISCARD` cap, and trim's sort/dedup/runs vectors are deleted rather
than left beside the new path.

### M3 — the scheduler. **planned**
`maintain.rs`: `VolumeMaintenance`, the action set, the priority order, the
deadline, the pace, the escalation cadence. Host-pure, no runner yet.
*Acceptance:* tests 1, 3, 16.

### M4 — the driver ABI facet. **planned**
`FilesystemMaintenance`, the `KernelFs` bound, the ARXFS implementation, the
honest defaults, wrapper forwarding, conformance.
*Acceptance:* test 13.

### M5 — the runner. **planned**
The kernel maintenance task, the wait queue and its triggers, per-volume
authority and audit, the pressure-governed chunk, round-robin across mounts.
*Acceptance:* tests 7b, 8b, 10, 11, 12, 14.

### M6 — escalation, reporting, and the command app. **planned**
The check-requested mark, the read-only downgrade, the sysinfo maintenance
state, and the `arxfs` command app with its Help tree.
*Acceptance:* tests 9, 15.

### M7 — acceptance and docs. **planned**
Last only if §13 is empty by the time it is reached; a defect still open there
takes the stage ahead of it (§13).
On-hardware measurement (a Pi 4 SD card's discard actually issuing, a paced
scrub's foreground impact measured against the writeback baseline); spec §24
generated from this file, the §2 feature table row, the §18 stage-18 row,
`docs/src/filesystem/arxfs.md`, the driver `README.md`, the `README.md`
feature/architecture matrix, and `plans/OPEN-DEFECTS.md` D64 closed; this file
replaced by its done-state summary.
*Acceptance:* the foreground-impact share is recorded as a measured number, not
a claim; the combined floor passes with bounded resident bytes.

## 17. Non-goals

- **A second scheduler.** FEC's job engine and this one share the pacer; the
  block-layer array scheduler stays where it is. Three schedulers pacing to
  three notions of busy is the defect §6 exists to prevent.
- **Background sparse conversion, background dedupe, or background
  defragmentation.** Each rewrites data. They are optimisations, not health, and
  mixing a data rewriter into the health scheduler makes both harder to reason
  about. Reporting the opportunity is in scope; acting on it is not.
- **Automatic `check`, automatic aggressive repair, automatic `rescue`.** §10.
- **A tunable.** No mount option, no settings file, no environment variable, no
  build feature that changes the cadence or disables maintenance.
- **A user-space maintenance *service*.** The runner belongs beside the mount it
  serves; a separate service would need a capability to reach another
  principal's volume, which is the ambient authority the model forbids. When
  ARXFS moves to a user-space driver process the runner moves with it.
- **Predictive failure analysis.** The runner reports telemetry and crossings
  against documented thresholds; it does not model a device's remaining life.
