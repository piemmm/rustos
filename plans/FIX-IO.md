# FIX-IO.md — Storage/media I/O fault isolation and recovery grace

Binding plan under `AGENTS.md`. One failing, stalling, or resetting storage
device — or a wedged hub/controller serving several of them — MUST NEVER lock
up the filesystem "strata" or the system. A faulting device is contained to
its own fault domain; every other mount, and the rest of the system, keeps
running unimpeded. A device that is only *briefly* unwell (a comms blip, a bus
reset, a hub that is mid-reset) is given a bounded **grace window** to come
back before anything is failed closed, because such blips are transient far
more often than they are terminal. This is the storage-side realisation of the
operating-conditions floor (§26, especially §26.5 "a disk may be failing" and
§26.1 "a slow device must never stall a fast one") and the no-busy-wait
mandate (§2.23).

This plan is staged so an AI can action **one full stage at a time** with no
surprises: each stage is self-contained, lands with its own tests, and leaves
the whole-project validation gate (§7) green on its own. Stages are ordered so
that the earliest ones already stop the lockup; the later ones deepen the
model. The plan states the current design and what remains (§13); it is not a
changelog.

---

## 0. Root cause (why the strata can lock up today)

The lockup is **structural**, not a bug in one driver. Verified against the
tree:

- **The block seam is an unbounded synchronous call.** A consumer drives a
  device through `tairix_abi::driver::block::Block`, implemented by
  `RemoteBlock` over the `BlkCall` seam (`drivers/storage/volmgr/src/blk.rs`).
  `BlkCall::call(request, reply, window)` issues `ipc_call` on the granted
  block endpoint. `ipc_call` (`lib/abi/src/syscalls.rs`, 5 args: endpoint,
  request ptr, request len, reply ptr, reply cap) has **no `timeout_ns`/
  deadline argument**, so the caller parks until the driver replies.
- **A wedged device never replies.** The per-LUN serve path
  (`drivers/storage/usb_msd/src/serve.rs`, `serve_request`/`serve_decoded`) is
  a pure one-request-in / one-completion-out function. If the underlying
  SCSI/BOT/UAS transfer to the medium stalls, no `BlkCompletion` is ever
  framed and `call_reply` is never issued — the consumer waits forever.
- **The completion vocabulary has no health axis.** `BlkCompletion`
  (`lib/abi/src/blkio.rs`) is either a geometry/success payload or a negated
  `Errno` (`encode_error_completion` / `decode_completion`). There is no
  "transient vs permanent", "retrying", "resetting", or "degraded" outcome, so
  a consumer cannot tell a disk that will come back from one that is dead, and
  cannot make an isolation decision. `errno_to_driver` (`blk.rs`) collapses
  everything it does not specifically recognise to `DriverError::DeviceFault`.
- **Fault domains are not modelled for I/O stalls.** The hardware tree
  (`lib/abi/src/hwtree.rs`) captures topology and the D4 surprise-removal path
  (`plans/DEVICES.md`; `MountAvailability::{UnavailableDirty,UnavailableLost,
  RecoveryConflict}` in `lib/abi/src/sysinfo.rs`) handles a device that
  physically *vanishes* — but nothing handles a device that is still present
  yet unresponsive, and nothing ties a hub/controller reset to a coherent
  quiesce/resume of the block endpoints beneath it. A hub serving several
  disks blipping looks like N independent, permanent disk failures.

The primitives to fix this already exist and are reused, not reinvented (§2.2):
per-driver user-space processes (§4 isolation), wait-sets
(`lib/abi/src/waitset.rs`; `waitset_wait(handle, timeout_ns, token_out)`,
`u64::MAX` = no timeout), `irq_bind`/`irq_wait(handle, timeout_ns)`, the
`-TimedOut` errno convention (`hw_tree_wait`/`irq_wait`/`waitset_wait`), the D4
retained-writes/verified-re-insert machine, the `lib/log` hash-chained audit
trail (§19.4), the `sysinfo` health precedents (bond members; surprise-removed
volumes that "never masquerade as healthy"), and the watchdog's driver-restart
recovery (`plans/WATCHDOG.md`).

---

## 1. Invariants (hold in every stage, on every Tier-1 target)

1. **Every I/O is time-bounded, end to end.** No consumer path may block
   indefinitely on hardware. A per-request deadline turns an infinite wait into
   a deterministic, fail-closed event — the mechanism, not a hack (§2.1's ban
   on "retry-until-it-works", §26.5).
2. **Isolation by construction.** Each block device is served by its own
   user-space process (§4); a fault is contained to that address space
   (§19.5 blast-radius). The consumer side never serialises unrelated devices
   behind one blocking thread (§26.1 head-of-line freedom).
3. **A blip is ridden out, not punished — within a bounded grace window.** A
   device that stalls, resets, or whose hub is mid-reset is held `Recovering`
   for a bounded, event-timed grace period; if it returns inside the window,
   I/O resumes and the episode is logged as a recovery, not a failure. Only
   after the window expires without recovery is the device failed closed. The
   grace window is *policy* (Stage IO3), sized per device class — never one
   global `const` (§24.1) and never a security/validation bound (§24.4).
4. **Health is first-class and honest.** A device has an explicit state
   machine distinguishing transient from permanent and recovering from failed.
   A returning disk is a normal, logged transition (§26.5). A device that has
   faulted stays quarantined-but-recoverable until it *demonstrably* recovers,
   so a flapping disk cannot masquerade as healthy (the `sysinfo` precedent),
   yet recovery never needs a reboot (§18.4).
5. **Fault domains are a discovered tree.** A device node's parent bus/hub/
   controller is its fault-domain owner, read from the hardware tree, never
   hard-coded (§18.1, §2.20). USB hub, SAS/JBOD expander, and PCIe root complex
   are all just interior nodes.
6. **Never busy-wait (§2.23).** All waiting — per-request deadline, grace-window
   timing, retry backoff — is event-driven (`waitset_wait`/`irq_wait` relative
   timeout, one-shot timer). Backoff parks on a timer; it never spins.
7. **Fail closed, degrade gracefully (§5.4, §2.24).** At the boundary of what we
   can vouch for (timeout, corrupt reply, checksum failure) we return a typed
   error and never serve data we cannot trust; the *session/system* survives,
   only the *operation* fails.
8. **No self-compat, no dead code (§2.13, §2.14).** The deadline-less block
   path and the success-or-`errno`-only `BlkCompletion` are replaced in place,
   with every consumer updated in the same change. No v1/v2 seam, no shim.

---

## 2. Stage plan

Each stage is complete and green on its own (§2.19). IO1–IO2 already stop the
lockup; IO3 adds the grace window this issue is about; IO4–IO6 deepen the
model. IO6 depends on ARXFS existing and is gated on it.

### Stage IO1 — Bounded, cancellable block transport. **done**

Give the block seam a deadline so a consumer is *never* parked forever on a
wedged device. The ABI is unfrozen (§9), so change it in place (§2.13).

**Landed (now guaranteed):** the async submit/complete seam exists as three
non-blocking syscalls on the existing `CallEndpoint` — `call_post` (99,
posts + arms a per-ticket one-shot deadline + writes the ticket out),
`call_reap` (100, non-blocking claim: `-WouldBlock`/`-TimedOut`/`-NotFound`),
`call_cancel` (101, per-ticket withdraw) — plus the `WaitSourceKind::CallReply`
wait-set source (ready on a landed reply **or** an elapsed deadline). The
per-ticket deadline is threaded through `PendingCall`/`in_service` and
`CALL_WAITQ` is now a swept timed queue (`nearest_timed_deadline`), so a wedged
callee fails closed rather than parking forever. `BlkCompletion` leads with a
`BlkStatus` health word (`Ok`/`Degraded`/`TransientError`/`Timeout`/`Reset`/
`MediumError`/`Offline`/`Removed`/`Fatal`) decoded fail-closed to `Fatal`;
`DriverError` gained `MediumError`/`DeviceOffline` and `Errno` gained
`MediumError`(39)/`DeviceOffline`(40); `BlkDeviceClass::budget()` is the single
per-class deadline/retry/queue-depth policy. Consumers rewritten in place: the
kernel `blkclient` (deadlined, `TimedOut`→fail closed) and volmgr's `RtBlkCall`
(now `call_post`+`CallReply` wait-set+`call_reap`). `lib/rt` gained the three
wrappers; the C header and syscall-table hash regenerate through the drift
guards. The serving drivers (`usb_msd`/`virtio_blk`/`emmc2`) keep working on the
source-compatible codec and emit explicit health statuses only from IO3 on.

**Chosen transport shape (the reviewer decision this stage reserved): the
async submit/complete seam.** The deadlined-synchronous alternative is
rejected: it is still one-in-flight-per-caller (effectively a blocking
thread per device), which cannot satisfy IO2 head-of-line freedom (§26.1) or
IO3's "hold in-flight requests through a grace window" without being reworked,
so building it first would be the deferred-correctness §2.19 forbids. The
async shape is the complete abstraction (§27) and reuses the existing
ticketed call machinery and wait-set event loop (`kernel/ipc/src/call.rs`,
`lib/abi/src/waitset.rs`, `plans/USB.md`) rather than inventing a parallel one
(§2.2).

Deliverables:

- **A general-purpose *asynchronous call* on the existing `CallEndpoint`.**
  The client half of `ipc_call` today bundles post + park + reap with no
  deadline. Split those into three non-blocking syscalls over the *same*
  endpoint object every service already binds (`call_create`/`call_recv`/
  `call_reply`), so this is one IPC primitive completed (§27), not a
  block-only bolt-on:
  - `call_post(endpoint, request_ptr, request_len, ticket_out_ptr,
    deadline_ns)` — posts without blocking (`CallEndpoint::post`), writes the
    minted `CallTicket` to `ticket_out`, wakes the bound server, and arms a
    per-ticket one-shot deadline. `deadline_ns == u64::MAX` means "no
    deadline" (the `waitset_wait`/`irq_wait` convention). Same capability +
    grant + size checks as `ipc_call` (no new authority; the endpoint's
    send-cap and any per-endpoint grant are re-checked kernel-side, §5.4).
  - `call_reap(endpoint, ticket, reply_ptr, reply_cap)` — non-blocking
    `take_reply`: copies the reply out and returns its length on
    `Ready`; `-WouldBlock` on `Pending`; `-TimedOut` if the ticket's deadline
    has passed (the kernel retires the ticket and, best-effort, cancels the
    in-flight request); `-NotFound` on `Cancelled`/`Unknown` (endpoint torn
    down, or not this caller's ticket — no existence oracle, §5.4).
  - `call_cancel(endpoint, ticket)` — withdraw one outstanding request
    (a per-ticket form of the existing `cancel_posted_by`), so a consumer
    abandoning a wedged transfer frees the endpoint slot deterministically.
  - **Wait-set completion source: `WaitSourceKind::CallReply`** (`id` = the
    endpoint id). Added to a wait-set the consumer owns; owner-checked at
    add time by the caller's send authority to that endpoint (the `ipc_call`
    grant check), never the endpoint *owner* check (the caller is the client,
    not the server). Readiness is the existing non-consuming peek:
    `CallEndpoint::has_ready_reply_for(claimant)` — a reply the caller posted
    is `completed` and unclaimed, **or** its deadline has elapsed (so a
    timeout wakes the waiter exactly like a real completion). The woken
    consumer drains with `call_reap`, never the wait.
  - New `CallEndpoint` methods (host-unit-tested in `kernel/ipc`):
    `has_ready_reply_for(claimant)`, `cancel_one(claimant, ticket)`, and a
    per-ticket `deadline` recorded in `PendingCall`/`in_service` so
    `take_reply`/reap can surface `-TimedOut`. The deadline is armed through
    the same timed wait-queue the kernel already uses (`waitq`), so it is
    event-timed, never a spin (§2.23).
- **`BlkCompletion` gains an explicit outcome/health axis** in
  `lib/abi/src/blkio.rs`, replacing the bare success-or-`-errno` frame with a
  leading `BlkStatus` word: `Ok`, `TransientError` (retryable — recovered
  ECC, comms glitch), `Timeout`, `Reset` (aborted by a device/hub reset; safe
  to reissue), `Degraded` (served, but the device reports itself unhealthy),
  `MediumError` (permanent bad-sector), `Offline`/`Removed` (surprise
  removal, already precedented in `sysinfo.rs`), `Fatal`. `Timeout` is
  synthesised kernel-side by the `call_reap` deadline path (the serving
  driver need not answer to produce it); the others the serving driver emits
  from what it already knows (SCSI sense → `MediumError`, surprise-removal →
  `Offline`/`Removed`, recovered error → `TransientError`, device health →
  `Degraded`). Decoded fail-closed: an unknown status word is `Fatal`, never
  silently `Ok`.
- **`errno_to_driver` / `DriverError` grow the matching typed classes**
  (`DriverError` is the `#[repr(i32)] #[non_exhaustive]` enum in
  `lib/abi/src/driver/mod.rs`): add `MediumError`, `DeviceOffline` (device
  present but unresponsive/removed), and reuse `Busy`/`DeviceFault`/
  `EndpointStalled` for the transient/reset classes, each with a distinct
  `Errno` so the filesystem layer can act (transient/reset → reissue;
  medium/offline → surface as I/O error). No status collapses to a generic
  `DeviceFault` (root cause #3).
- **Per-device bounds are policy, not a global `const`** (§24.1 vs §24.4): the
  deadline, retry count, and queue depth are derived from the device's
  discovered class (a rotational SATA disk ≠ an NVMe namespace) and fail closed
  under pressure (§26.1). The class → budget mapping lives in one place both
  the serving driver and the consumer read, never a copied literal (§2.2).

Consumers rewritten in place (no shim, §2.13): the kernel-side block client
(`kernel/core/src/fs/blkclient.rs`), the volume manager's `RemoteBlock`
(`drivers/storage/volmgr/src/blk.rs` — its `BlkCall` seam becomes
submit/reap-on-a-wait-set while the synchronous `Block` trait it exposes to
filesystems is preserved, so the filesystems are unchanged), and the serving
side (`drivers/storage/usb_msd/src/serve.rs`, plus the `virtio_blk`/`emmc2`
serve paths) to emit `BlkStatus`. The `lib/rt`/`lib/drvrt` wrappers, the
syscall table (`lib/abi/src/syscalls.rs` → `kernel/syscall/src/table.rs`),
the generated C header, and the fuzz/proptest syscall models gain the three
new calls; `cargo xtask abi-check`/`c-header` enforce the drift guards.

Tests (§7): `kernel/ipc` unit tests for `call_post`/`call_reap`/`call_cancel`
+ deadline + `has_ready_reply_for` (round-trip, `-WouldBlock`, `-TimedOut`,
per-ticket cancel, claimant mismatch fails closed); `kernel/core` wait-set
tests that a `CallReply` member wakes on a ready reply and on a deadline; the
`blk.rs` host doubles — a serving double that stalls past the deadline yields
`Timeout` and the consumer unblocks; round-trip and fail-closed decode of
every new `BlkStatus`; fuzz the completion/status decode and the reap/deadline
path (§19.6).

### Stage IO2 — Consumer-side isolation (volmgr + filesystems). **in progress**

Stop the "entire strata locks up": no shared blocking thread across devices.

The structural isolation is already in place from the IO1 consumer rewrites and
is *not* re-architected here: there is no single serialised worker fanning out
to all disks. Each mount has its own served block device, its own transport
(the kernel `BlkClient` over a per-LUN `CallEndpoint`; volmgr's `RtBlkCall` over
a per-device `CallReply` wait-set), and the mounted filesystem service takes a
**per-driver** `SleepLock` (never a global one), so a per-request deadline park
on a wedged device parks only that mount's callers — unrelated volumes run on.

Landed:
- **Consumer fault-awareness via one shared mapping.** Every consumer of a
  served block device classifies a completion's health through the single
  `DriverError::from_errno` (`lib/abi`), never a per-consumer copy (§2.2). A bad
  sector (`MediumError`), a gone/unresponsive/removed device (`DeviceOffline`),
  a transient stall or reset (reissuable `Busy`), and a timed-out/vanished
  endpoint (fail-closed `DeviceFault`) each keep their distinct class, so a
  fault on one device surfaces only to *its* callers. volmgr's `RemoteBlock` no
  longer collapses the health axis to `DeviceFault`.
- **Consumer-side bounded reissue** — the consumer half of the reply-reissuable
  model (IO3). Both consumers (the kernel `BlkClient`, volmgr's `RemoteBlock`)
  reissue a driver-framed reissuable completion (`is_retryable`:
  `TransientError`/`Reset`/`Timeout`) a bounded number of times before failing
  closed, so a device that is merely recovering is not punished with a spurious
  I/O error. The cap is the shared per-class policy `IoBudget::max_retries`,
  read through one definition `IoBudget::should_reissue(status, attempts)` both
  consumers call (§2.2), so they cannot drift apart; a device that keeps
  answering reissuably still fails closed deterministically at the budget (no
  retry-until-it-works, §2.1). Each reissue is a fresh post → park-on-reply
  exchange (event-driven, never a spin, §2.23); the serving driver owns the
  grace window and its timers. A hard per-request deadline miss and a torn-down
  endpoint fail closed with no reissue; a non-retryable verdict (`MediumError`/
  `Offline`/`Removed`) surfaces on the first attempt.
- Object-level isolation regression test (volmgr host doubles): a faulted
  device (offline) beside a healthy one — the faulted client fails every read
  closed with the typed health error while the healthy sibling keeps serving
  correct data, interleaved. Reissue regression tests on both consumers: a
  transient blip that resolves inside the retry budget is ridden out (correct
  data returned), and a device that resets on every attempt fails closed as
  `Busy` at the budget.
- **The affected volume is marked degraded, not just its callers.** The
  kernel `BlkClient` folds every completion's reported `BlkStatus` through the
  single shared `MountAvailability::from_block_status` mapping (`lib/abi`,
  §2.2) into a lock-free per-volume overlay handle it exposes
  (`health_handle`); the mount registry (`kernel/core` `mounted.rs`) attaches
  that handle per mount (`set_health_source`) and the mount snapshot overlays
  a live `Degraded`/`Recovering` reading onto an otherwise-`Available` volume
  (`overlaid_availability`), so `sysinfo`/`mount`/`df`/`sysmon` show a
  live-but-unwell volume distinctly (new `MountAvailability::{Degraded,
  Recovering}`). The authoritative D4 `Unavailable*`/`RecoveryConflict` vanish
  states always win over the overlay, so a vanished volume never masquerades
  as merely unwell; the serving driver still owns the sticky `BlkHealth`
  machine, so the consumer only reflects its verdict (no second, divergent
  state machine). Proven host-side: the status→availability mapping, the
  consumer fold, and the snapshot overlay (incl. stored-state precedence).

Remaining:
- A QEMU vertical with a wedged/removed virtio-blk or USB MSD device beside a
  live volume, asserting the live device's throughput is unaffected while the
  wedged one fails closed at its deadline (true concurrent head-of-line
  freedom, which the host doubles cannot express without the kernel deadline
  machinery).

### Stage IO3 — Per-device health state machine + the recovery grace window. **in progress**

This is the core of the issue: a bounded grace window to ride out a blip
before failing closed.

**Landed (the shared primitive + first serving consumer):** the per-device
health state machine and grace window live in one place both a serving driver
and a consumer read — `blkio::BlkHealth` / `BlkHealthState` in `lib/abi`
(§2.2), consuming the per-class `IoBudget::grace_ns` policy (`BlkDeviceClass::
budget`). `BlkHealth::observe(raw, now_ns)` folds each *device-level* outcome
into `Healthy → Degraded → Recovering → { Healthy | Faulted } → Offline/
Removed → Failed` and returns the `BlkStatus` the consumer is told; it is pure
and event-timed (the caller supplies the monotonic `clock_get` reading, no
timer to spin on, §2.23), so the whole machine is proven host-side. A
transient stall/reset inside the window is reported reissuably (`Reset`) and
held `Recovering`; the same stall once the window elapses is `Faulted` and
fails closed (`Offline`); any valid answer recovers the device
(sticky-but-recoverable, no reboot). Only device-level outcomes drive health —
`BlkStatus::for_driver_health` returns `None` for request-level rejections, so
a hostile/malformed request can never fault a healthy device. The **reply-
reissuable** recovery model (not inline parking) is the one this stage adopts,
so one unit's blip never stalls the serve loop's other units (head-of-line
freedom, §26.1) — exactly the "reply within the per-request deadline while the
device works its grace window behind the scenes" option the design below
allows. The whole request engine is one shared definition every block driver
reuses — `blkio::serve_request_recovering` in `lib/abi` (§2.2, §27): it decodes
and validates a request, drives the device through the `Block` trait, folds the
outcome into a `BlkHealth`, and frames the completion, with a
`Served{Device,Refused}` split so a request refusal is never fed to health.
Validation, the fail-closed refusals, the success paths, and the grace window
thus cannot diverge between drivers, and the engine is pure/alloc-free and
proven host-side over fault-injecting `Block` doubles in `lib/abi`. The first
consumer is `usb_msd`: its wait-set serve loop hands each per-LUN request to the
engine with that LUN's `BlkHealth` (the `Removable` class) driven by
`clock_get`; only the usb_msd-specific block-service endpoint-id derivation
(`serve::blk_block_for`) lives in the driver crate. The state
machine is also complete for the *quiet-device* case (§27): `BlkHealth::
grace_deadline_ns` gives the absolute one-shot deadline a driver arms its idle
timer to while `Recovering`, and `BlkHealth::poll(now_ns)` is the pure,
event-timed transition that fails a still-`Recovering` device closed to
`Faulted` when that window elapses with no further request to fold through
`observe` — the shared `grace_elapsed` check keeps `observe` and `poll` from
diverging (§2.2). The idle-timer wiring is landed in `usb_msd`: its serve loop
arms its wait's one-shot timeout from the shared
`blkio::recovery_wait_timeout` (the soonest armed grace deadline across its
LUNs, relative to `clock_get`) and folds every LUN's `BlkHealth::poll` on each
wake, so a LUN that stalls then goes quiet still fails closed on time (logged
once, node/endpoint kept so its consumer gets typed fail-closed answers and a
later genuine return recovers it — a health fault is not a surprise-removal).
The timeout arithmetic is that one shared helper, so `virtio_blk`/`emmc2`
reuse it unchanged when brought up (§2.2), never a per-driver copy.

The bounded recovery **escalation** — what the *driver* does to the hardware
between reissued attempts — is landed as its own shared primitive
`blkio::RecoveryLadder` (§2.2, §27), the driver-action counterpart of
`BlkHealth`. `RecoveryLadder::next_action(state)` maps a device's current
`BlkHealthState` to the next `RecoveryAction`: an operational device re-arms the
ladder (`None`); a `Recovering` device escalates — a gentle `Retry` first (a
one-off glitch often clears itself), then a data-path `Reset` on each subsequent
attempt — up to the class's `IoBudget::max_retries` (the **same** cap the
consumer's `should_reissue` reads, so escalation and reissue derive from one
policy and cannot drift), after which it is `GiveUp` and the grace window is left
to fail the device closed; a device already failed closed is `GiveUp` until a
demonstrated return re-arms it. The ladder holds no clock and never spins or
parks: it advances one rung per reissued attempt, which are already spaced by the
consumer's reissue cadence and per-request deadline — stronger than a driver-side
backoff timer for head-of-line freedom (§26.1), and provable host-side. `usb_msd`
is the first consumer: after each reply its serve loop consults the LUN's ladder
from the just-folded `BlkHealth` state and, on `Reset`, clears the unit's bulk
pipes (`ScsiDevice::scrub_window`, its one data-path reset) and logs an
`MSD_RECOVERY_RESET` audit event; the reset is only issued for a unit already
answered reissuably, so it never stalls an unrelated LUN. `virtio_blk`/`emmc2`
inherit the same primitive when brought up as user-space serve processes,
mapping `Reset` to their own queue/controller re-init.

Remaining:
- `virtio_blk` and `emmc2` are currently consumed **in-kernel** (root-unlock)
  and expose only their `Block` implementation — they are not yet brought up as
  user-space serving processes with their own serve loops. When either is, it
  reuses the shared `blkio::serve_request_recovering` engine (above) rather than
  copying it, so there is nothing to "adopt" separately; the shared engine is
  the single definition (§2.2). Bringing those serve processes up is tracked
  with their user-space driver-process work, not this stage.
- The consumer marking the affected volume degraded landed in IO2 above (the
  kernel `BlkClient` overlay surfaced through `MountAvailability::{Degraded,
  Recovering}`); the remaining observability is the audit-log health trail
  (Stage IO5).
- A QEMU vertical driving a device through fault → grace(recovering) →
  return-inside-window → Healthy and fault → grace-expiry → Faulted →
  fail-closed, which the host doubles cannot express without the live kernel
  deadline machinery.

Deliverables (design):
- **A per-device health state machine, owned by the block driver process:**
  `Healthy → Degraded → Recovering(grace) → { Healthy | Faulted } → Offline/
  Removed → Failed`. Transitions are logged (Stage IO5).
- **The grace window.** On the first stall/reset/comms error, the device enters
  `Recovering` and arms a **one-shot grace timer** (a relative-timeout
  `waitset_wait`, never a spin). While inside the window:
  - In-flight requests complete with `Reset`/`TransientError` (reissuable) or
    are held up to their per-request deadline (IO1) — never a hard `Fatal`.
  - New requests are briefly parked on the same event-timed budget rather than
    failed immediately, so a blip that resolves in milliseconds is invisible to
    the workload.
  - The driver runs its bounded recovery escalation through the shared
    `blkio::RecoveryLadder`: a gentle retry, then a bounded, escalating
    data-path reset (a driver maps `RecoveryAction::Reset` to its mechanism —
    `usb_msd` clears its bulk pipes; the fault-domain owner's hub/controller
    reset is Stage IO4). It is bounded by the class's `IoBudget::max_retries`
    (no unbounded retries, §2.1) and, in the reply-reissuable model, advances
    one rung per reissued attempt — spaced by the consumer's reissue cadence,
    so the driver neither spins nor parks a backoff timer of its own (§2.23,
    §26.1 head-of-line freedom).
  - **If the device returns inside the window** → `Recovering → Healthy`, held
    requests complete normally, and a recovery event is logged (§26.5 "disks can
    come back to life; note it in the health log"). This is the explicit
    ride-out-the-blip behaviour.
  - **If the window expires without recovery** → `Faulted`/`Offline`, and only
    *then* does the device fail closed to its consumers, feeding the existing D4
    retained-writes / verified-re-insert path (`plans/DEVICES.md`) so
    uncommitted data is preserved for a later verified return.
- **The grace duration is per-device-class policy** derived from discovered
  hardware (a rotational disk's spin-up/reset budget ≠ a bus glitch), with a
  sane default for desktop *and* server (§24.2), documented in the driver crate
  and its `docs/src/` page — never one global `const` (§24.1) and never a
  security/validation bound (§24.4).
- **Sticky-but-recoverable.** A device that has faulted stays degraded/
  quarantined until it demonstrably recovers, so a flapping disk cannot present
  as healthy (the `sysinfo` "never masquerades as healthy" precedent), yet a
  genuine return always recovers it without a reboot (§18.4).
- The driver owns its own timers/IRQ waits and never blocks the consumer: it
  either completes, or replies with a `Timeout`/`Reset`/`Degraded` completion
  within the per-request deadline while the device works through its grace
  window behind the scenes. The pure per-request logic is the shared
  `blkio::serve_request_recovering` engine in `lib/abi`, host-tested; each
  driver's serve loop wraps a recovery arm around it.

Tests (§7): drive the state machine over a fault-injecting `Block` double
through fault → degrade → grace(recovering) → **return-inside-window →
Healthy** (I/O resumes, recovery logged) *and* fault → grace expiry → Faulted →
fail-closed → D4 retention; assert the grace timer is event-timed (no busy
spin) and that requests inside the window are held/reissued, not hard-failed;
assert a flapping device stays sticky-degraded until a real recovery.

### Stage IO4 — Fault-domain tree (hub/controller quiesce/resume). **in progress**

A hub or controller blip is *one* fault-domain event, not N spurious disk
failures.

**Landed (the shared primitive):** the interior-node fault-domain state machine
lives beside the per-device one in `lib/abi` — `blkio::FaultDomain` /
`FaultDomainState` (`Healthy → Recovering(grace) → Offline`, sticky-but-
recoverable). It is the interior-node counterpart of `BlkHealth` and reuses the
**same** grace-window timer: the recovery-window arm/elapsed/one-shot-deadline
arithmetic was extracted into one private `blkio::GraceWindow` that both
`BlkHealth` and `FaultDomain` drive, so a leaf device and an interior node time
their grace window identically and cannot diverge (§2.2). `FaultDomain::quiesce`
opens one shared window over the whole subtree (children answered reissuably via
`child_status → Reset`); `resume` records a demonstrated owner return and
recovers the subtree to `Healthy` at once (no reboot, §18.4); `poll` fails a
`Recovering` subtree closed to `Offline` when the window elapses, driven by the
one-shot deadline `grace_deadline_ns` names (event-timed, never a spin, §2.23);
the failed subtree is sticky until a demonstrated return so a flapping hub never
masquerades as healthy. The domain stores only the owner's opaque hardware-tree
node id, so the type is platform-neutral (§2.20) and children are read from the
discovered tree (`hwtree.rs`), never hard-coded (§18.1). The whole machine is
pure and event-timed (the caller supplies the monotonic reading and drives the
children's own `BlkHealth`), so the coherent quiesce/resume is proven host-side.

**Landed (the tree association):** the pure resolution of *which* interior node
owns a device's fault domain is the shared, host-tested primitive
`hwtree::fault_domain_owner(nodes, node_id)` in `lib/abi` (§2.2, §18.1): it walks
the discovered hardware tree upward and returns the nearest strict ancestor that
owns a group of devices — a bus/hub/controller/expander/PCIe-root-complex
(`HwDeviceClass::Bus`) or the synthetic `Root` as the domain of last resort —
skipping non-owning ancestors. It is platform-neutral (reads the tree, hard-codes
no board, §2.20) and fails closed (`None`) on an absent
node, a rootless node, or a broken/cyclic chain (the walk is bounded by the node
count, never an unbounded spin, §2.9/§5.4). Proven host-side over a USB-shaped
tree (nearest bus, root fallback, nesting, non-owning-ancestor skip, and the
absent/broken/cyclic fail-closed cases). The **full ordered chain** of nested
fault-domain owners a device blips with (leaf → hub → controller → root, nearest
first) is the shared lazy iterator `hwtree::fault_domain_chain(nodes, node_id)`
built by re-applying `fault_domain_owner` to each owner in turn, so a serving
driver builds one `FaultDomain` per interior node without re-deriving the walk
itself (§2.2). It is allocation-free (holds only a borrow of the tree, so no
fixed-depth ceiling, §24.1), inherits `fault_domain_owner`'s fail-closed
behaviour at every level, and is cycle-safe — bounded to at most one step per
node, so even a bus pair that parents each other terminates rather than spins
(§2.9). Proven host-side (the nested chain nearest-first, the interior-node and
root-fallback starts, the empty chain of the root itself, the non-owning-ancestor
skip, the absent/broken fail-closed cases, and the bounded cyclic-bus walk).

**Landed (the composition primitives — the shared fold + timing the live
wiring consumes):** two pure helpers in `lib/abi` let a serve loop use its
fault domains exactly as it already uses the per-device machinery, so the live
wiring cannot re-derive the rules (§2.2, §27):
- `BlkStatus::severity` + `BlkStatus::combine` are the single, explicit
  definition of *which health signal wins* when more than one applies to one
  request. `severity` ranks the vocabulary healthy → served-but-unwell →
  reissuable → permanent → gone (`Ok` < `Degraded` < `TransientError` <
  `Timeout` < `Reset` < `MediumError` < `Offline` < `Removed` < `Fatal`), kept
  deliberately independent of the wire value `as_u32` so the transport encoding
  and the recovery precedence can never silently couple; `combine` is the
  more-fail-closed of two, a total order so the fold is associative/commutative.
- `blkio::effective_child_status(device_status, domains, now_ns)` folds a leaf
  device's own outcome with what each ancestor fault domain imposes
  (`FaultDomain::child_status`, over the chain `fault_domain_owner` resolves)
  into the one status the child's completion carries. A hub mid-reset turns a
  child's `Ok` into a reissuable `Reset` (aborted data not consumed); a
  window-elapsed ancestor fails the child closed to `Offline`; a device's own
  definitive `MediumError` still wins over a concurrent reset; and a deep
  failing domain is never masked by a shallow healthy one.
- `blkio::fault_domain_wait_timeout(domains, now_ns)` is the interior-node
  counterpart of `recovery_wait_timeout` — the soonest armed subtree window,
  relative to now. Both now delegate to one private `nearest_relative_deadline`
  core, so a loop owning both per-device and fault-domain windows takes the min
  of the two and cannot time them by different rules.
All three are pure and proven host-side (severity total-order + combine lattice
laws; the child-status fold with precedence and nesting; the interior-node
timeout mirroring the per-device one).

**Landed (the first live wiring — the `usb_msd` shared-transport fault
domain):** a fault-domain owner need not be a *bus* node in the tree — a leaf
driver's own shared transport that fans out to several logical units is equally
a fault-domain owner of those units (the `FaultDomain` doc now states this), and
`usb_msd` is the first live consumer. Every LUN behind one Bulk-Only / UAS
device shares the *same* bulk pipe pair, so the data-path reset the recovery
ladder escalates (`ScsiDevice::scrub_window`) is a transport-wide event, not a
per-LUN one. The serve loop now owns one `FaultDomain` for that shared transport
(owner = the device's own discovered URB transport grant, never a board
constant; the removable-class grace window). The per-request coordination is the
pure, host-tested `recover::serve_lun_with_domain` in the crate's `lib`: it
always drives the unit through `blkio::serve_request_recovering` (so a returning
transport is *discovered* — a definitive answer, `data_valid`/`MediumError`, is
the only demonstrated proof the shared pipes are back), recovers the whole
device on that proof (`resume`) before folding, then folds the transport
domain's verdict with `effective_child_status` (a sibling LUN's request during
the window is answered reissuably under the one shared window, failed closed to
`Offline` once it elapses) — re-framing only ever over a non-data-valid status,
so no valid read is discarded. The loop `quiesce`s the domain around the scrub,
arms its wait from the min of `recovery_wait_timeout` and
`fault_domain_wait_timeout`, `poll`s the domain on every wake, and audits the
device-wide edges through the shared `BlkHealthTransition::for_fault_domain`
vocabulary (`MSD_DOMAIN_RECOVERING 4171`, `MSD_DOMAIN_RECOVERED 4172`,
`MSD_DOMAIN_OFFLINE 4173`). So one shared-transport blip is one recovery episode
across the device, not N spurious LUN failures.

**Landed (the first live *interior hardware-tree* fault domain — the xHCI
controller):** the first bus/HCD driver to own an interior tree node's
`FaultDomain` is the xHCI host-controller driver (`drivers/bus/usb/xhci`). The
controller is the interior node every USB device below it hangs from, so a
controller-wide fault — a latched Host System Error / HCHalted, or the
`USBCMD.HCRST` reset the driver performs to recover — is one recovery episode
over the whole subtree, not one spurious failure per device. The pure,
host-tested coordinator `domain::ControllerHealth` (the crate's `lib`) wraps one
`FaultDomain` (owner = the controller's own discovered URB endpoint-block base,
never a board constant; grace = the documented `CONTROLLER_GRACE_NS`, matching
the removable-storage window it sits above so the controller and the storage
beneath it ride a blip out under one coherent budget) and encapsulates the
recovery sequencing over the landed primitives: `begin_recovery` opens the
shared window on the first fault, `note_reset(ok)` recovers on a demonstrated
return or advances the window on a failed reset, `poll` fails a quiet recovering
controller closed on the one-shot deadline `wait_timeout` names, and
`is_failed_closed` declares a dead controller so it is not retried forever
(sticky-but-recoverable — a later successful reset clears it). The freestanding
serve loop drives it around the existing synchronous
`recover_if_controller_faulted` (the `recover_controller` wrapper), arms its wait
from `wait_timeout` (previously always `WAIT_FOREVER`), and retries on the grace
one-shot — the fix for the real gap that a faulted controller raises no further
interrupt (xHCI §4.24.1), so a failed reset previously left the event loop
parked forever with no timer to retry it. Device-wide edges are audited through
the shared `for_fault_domain` vocabulary (`HCD_DOMAIN_RECOVERING 4190`,
`HCD_DOMAIN_RECOVERED 4191`, `HCD_DOMAIN_OFFLINE 4192`). The controller-node
machine is proven host-side; the serve-loop wiring is metal-only (QEMU models no
Pi USB).

**Landed (the cross-process propagation signal):** an interior fault-domain
owner's health is now a first-class, reactive **hardware-tree node property** —
`HwNode::fault_health` (a `FaultDomainState` byte on the wire, one shared codec
`FaultDomainState::as_u8`/`from_u8_fail_closed`, unknown decodes fail-closed to
`Offline`). A bus/hub/controller driver publishes its *own* node's health
through the new `hw_node_health` syscall (number 102, arg = the
`FaultDomainState` discriminant, `CAP_HW_EMIT`, audited): the kernel resolves
the caller's own matched node from its task id (no forging another node's
health), records it on the live tree, and bumps the generation so the reactive
`hw_tree_wait` observers re-read — the *same* channel the emit/remove hotplug
path uses, but a **distinct** signal (the node stays present, only its health
changes, so a merely-recovering subtree is never torn down). The live emitter
is the xHCI controller: its `log_domain_event` maps each `ControllerHealth`
edge to `Recovering`/`Healthy`/`Offline` and publishes it best-effort (the
audit record is authoritative; a refused publish never fails the recovery).
The live consumer is the device manager: a bound child whose fault-domain
**owner** (its nearest bus/hub/controller ancestor, recorded per binding as
`NodeDriver::owner` via `fault_domain_owner` at bind time) is currently
`Recovering` is **held**, not unloaded, when it transiently vanishes — so one
controller reset is one recovery episode across the subtree rather than N
spurious teardown/reload cycles; the child unloads only once the owner is no
longer recovering (returned `Healthy` without the child, or the subtree failed
closed). The kernel `BlkClient` already marks each affected volume
`Degraded`/`Recovering` as the leaf transport blips (IO2/IO5), so the volumes
under a recovering controller surface as recovering through the existing fold.
Proven host-side: the `HwNode` health wire round-trip + fail-closed decode; the
`FaultDomainState` codec; the `HwTreeStore::set_node_health` record/generation/
fail-closed-`NotFound`; the `hw_node_health` handler (own-node resolution,
fail-closed out-of-range, no-loaded-node denied); and the device manager's
recovery-hold (`a_child_of_a_recovering_owner_is_held_not_unloaded`).

**Landed (the leaf-side multi-owner fold — a leaf attributes its ancestors'
published blip to the fault domain):** the read side of the cross-process
signal now reaches the *block completions themselves*, not just the device
manager's teardown decision. A leaf block driver folds the published
`FaultDomainState` of its whole interior-ancestor chain into each completion,
so one controller/hub reset is attributed to the fault domain — the leaf's
completions carry a reissuable `Reset` (or `Offline` once an ancestor has
failed closed) instead of N spurious per-LUN faults — rather than each LUN's
own `BlkHealth` degrading independently. The primitives:
- `FaultDomainState::imposed_child_status` (`lib/abi`) is the *one* owner-health
  → child-status rule (`Healthy` imposes nothing, `Recovering → Reset`,
  `Offline → Offline`); `FaultDomain::child_status` now resolves its grace
  window and delegates to it, so an owned (clocked) domain and a published
  (already-resolved) ancestor state impose identically (§2.2).
- `hwtree::ancestor_imposed_status` (slice) and
  `hwtree::ancestor_imposed_status_from_snapshot` (the wire snapshot a driver
  reads with `hw_tree_read`, alloc-free) fold the chain's published health;
  both share one traversal (`resolve_owner`/`fold_ancestor_status` over a
  generic per-id `lookup`), so the byte- and slice-backed folds cannot diverge,
  and a malformed/truncated snapshot degrades safe to `Ok`. Proven host-side:
  the mapping, owned/published agreement, the snapshot↔slice agreement across
  every health arrangement, deep-not-masked-by-shallow, and fail-closed cases.
- A driver learns *its own* place in the tree through the new `hw_self_node`
  syscall (number 103, `lib/abi` + `kernel/core` + `lib/rt`/`lib/abi-sys` +
  generated C header): the kernel resolves the caller's own matched node from
  its task id (never caller-supplied — no ambient authority, no global-tree
  window), needs no capability (self-identity baseline, like reading one's own
  pid), and fails closed `NotFound` for a task with no matched node. Proven
  host-side (own-node resolution + fail-closed).
- `usb_msd` is the first live consumer: `recover::serve_lun_with_domain` now
  folds an ancestor status supplied by a **lazy closure** invoked *only on the
  recovery path* (a stall the device did not answer definitively —
  `transport_alive` gates it out), so a healthy hot-path transfer never reads
  the tree (§2.16); a data-valid or medium answer proves the path above is up
  and always wins, so no valid read is ever masked. The serve loop fetches
  `hw_self_node` once and its closure reads the current snapshot into a bounded
  degrade-safe buffer and calls `ancestor_imposed_status_from_snapshot`. Proven
  host-side (recovering ancestor → reissuable, offline ancestor → closed,
  medium/data wins, offline ancestor dominates a recovering transport).

Remaining:
- A QEMU vertical of a modelled hub/controller with several children resetting,
  asserting one recovery episode across the subtree (the device manager holds
  the children through the owner's grace window rather than unloading them, and
  the leaf's completions carry the attributed reissuable status). A watched hub
  / SAS expander *owning its own tree node* (publishing its own
  `hw_node_health`) extends the xHCI single-owner emitter shape; the leaf fold
  already composes an arbitrarily deep published chain.

Tests (§7): the pure `FaultDomain` machine is proven host-side in `lib/abi` —
one owner reset holds the whole subtree reissuable under one window; an owner
returning inside the window recovers the subtree leaving no scar; an owner that
outlasts it fails the subtree closed; a quiet domain expires on the one-shot
time poll; a continuing reset cannot postpone the fail-closed; a failed subtree
is sticky until a demonstrated return. The `usb_msd` shared-transport wiring is
proven host-side over `recover::serve_lun_with_domain` (a healthy transport
passes a unit's own status through; a quiesced transport holds a stalling
sibling reissuable under one window; any unit's definitive answer — data or a
medium error — recovers the whole device; the elapsed window fails a sibling
closed; a return after the window still recovers). The xHCI controller
interior-node wiring is proven host-side over `domain::ControllerHealth` (a
first fault enters recovery and arms a one-shot; a continuing fault does not
postpone the fail-closed; a reset inside the window recovers; a failed reset
past the window and an idle poll each fail closed; a failed-closed controller
recovers on a later successful reset; a spurious success on a healthy controller
is silent). A QEMU vertical of a modelled hub with several children resetting
(asserting one recovery episode across the subtree) lands with the cross-process
propagation above.

### Stage IO5 — Health observability (audit log + `sysinfo`). **in progress**

**Landed (the mount-availability half):** the `sysinfo` mount table already
distinguishes a live-but-unwell volume — `MountAvailability::{Degraded,
Recovering}` (`lib/abi/sysinfo.rs`) are surfaced by the IO2 consumer overlay
(the mount snapshot's `overlaid_availability`), and `mount`/`df`/`sysmon`
render them with a `[degraded]`/`[recovering]` marker while the vanish states
stay authoritative. The C-ABI view (`include/tairix/tairix_sysinfo.h`)
regenerated through the drift guard.

**Landed (the consumer-side health audit trail):** the shared edge classifier
`MountAvailability::health_transition(prev, next) -> Option<BlkHealthTransition>`
(`lib/abi/sysinfo.rs`) is the single definition of *when a served volume's
health materially changed* — `Degraded` / `Recovering` / `Recovered` — and is
edge-triggered (an unchanged state, and any transition touching a
surprise-removal vanish state, yields no event, so D4 removals are never
double-counted and a re-insert never fabricates a recovery). The kernel block
client (`kernel/core/src/fs/blkclient.rs`) folds each completion's reported
`BlkStatus` through it via an atomic swap that yields the prior state, and emits
exactly one `lib/log` record per real edge through the audit sink it already
holds: `AuditEvent::{VolumeDegraded (4130, Warn), VolumeRecovering (4131, Warn),
VolumeRecovered (4132, Info)}` (`kernel/core/src/audit.rs`), each naming the
block-service endpoint (`dev`) and never a secret. A recovery — "the disk came
back" — is logged as an `Info` recovery, not a fault. Proven host-side: the
classifier (edge-triggering + vanish-state exclusion), an end-to-end
recovering→recovered trail over the live transfer path, and a direct
edge-triggered journey (degrade, duplicate-suppressed, recovering, recovered,
medium-error-no-event, degrade). Level-dependent tests are serialised through
one shared `tairix_kernel_core::test_sink::with_log_level` guard so they cannot
flake against a concurrent global-threshold change.

**Landed (the serving-driver device-level health half — `usb_msd`):** the
device-side counterpart of the mount-side classifier is the **same** vocabulary,
one definition (§2.2): `BlkHealthTransition::for_device(prev, next) ->
Option<BlkHealthTransition>` (`lib/abi/sysinfo.rs`) maps a serving driver's own
`BlkHealthState` edge to the shared `Degraded`/`Recovering`/`Recovered` events,
edge-triggered, excluding both the fail-closed edges (the grace window
elapsing — the driver's own distinct event) and every edge touching `Removed`
(owned by the D4 hotplug path), so a driver process and the kernel block client
cannot classify a recovery or a degrade differently. It is pure and proven
host-side (the shared-vocabulary mapping, edge-triggering, and
fail-closed/removal exclusion). The first serving consumer is `usb_msd`: its
serve loop snapshots each LUN's health around `serve_request_recovering` and
folds every idle grace-window `poll` through one shared `note_health_edge(before,
after, node_id)` helper, emitting exactly one `lib/log` record per real edge,
each naming the LUN's fault-domain node (`node_hex`) and never a secret —
`MSD_HEALTH_DEGRADED (4168, Warn)`, `MSD_HEALTH_RECOVERING (4169, Warn`, the
grace-window entry`)`, `MSD_HEALTH_RECOVERED (4170, Info`, "the disk came
back"`)`, with the fail-closed edge remaining the existing `MSD_GRACE_EXPIRED
(4166, Warn)` in the driver's own event-id range. `virtio_blk`/`emmc2` reuse the
same classifier and `note_health_edge` shape when brought up as user-space serve
processes (§2.2), never a second definition. The `usb_msd` per-device retry/reset
event (`MSD_RECOVERY_RESET`) and the shared-transport fault-domain edges
(`MSD_DOMAIN_{RECOVERING,RECOVERED,OFFLINE}`, classified through
`for_fault_domain`) also landed with the IO4 leaf-transport wiring; only the
*interior hardware-tree node's* own quiesce/resume events (a bus/HCD driver)
remain (IO4 remaining), emitted against this same vocabulary.

**Landed (the fault-domain-node classifier — the shared vocabulary's third
member):** the interior-node counterpart of `for_device` is
`BlkHealthTransition::for_fault_domain(prev, next)` (`lib/abi/sysinfo.rs`),
completing the shared health-event vocabulary so a hub/controller reset, a leaf
device blip, and a mount overlay cannot classify a recovery differently (§2.2,
§27). It maps a `FaultDomainState` edge to the shared events: a quiesce
(`Healthy → Recovering`, and defensively a re-entry from `Offline`) is
`Recovering`; the owner demonstrably returning (`Recovering | Offline →
Healthy`, incl. a `resume` clearing a failed subtree with no reboot) is
`Recovered`; the fail-closed edge (`→ Offline`, the grace window elapsing) and
every unchanged state yield `None` — the fail-closed edge is the fault-domain
driver's own distinct event, exactly as `for_device` excludes its fail-closed/
removal edges. An interior node has no degraded-but-serving state of its own, so
it never emits `Degraded`. Pure and proven host-side (the shared-vocabulary
mapping, edge-triggering, fail-closed exclusion, and never-`Degraded`).

Remaining deliverables:
- **Interior-node fault-domain health events** (`plans/SYSLOG.md`): the first
  *hardware-tree* interior node's quiesce/resume/grace-expiry events **landed**
  with the xHCI controller wiring (IO4): the HCD serve loop emits
  `HCD_DOMAIN_RECOVERING 4190` / `HCD_DOMAIN_RECOVERED 4191` /
  `HCD_DOMAIN_OFFLINE 4192`, naming the controller's owner id and classified
  through the shared `for_fault_domain` vocabulary (the fail-closed edge its own
  distinct event). Remaining are the *other* interior nodes' events (a watched
  hub, a SAS/JBOD expander), emitted against this same vocabulary, landing with
  their live serve-loop / propagation wiring (IO4 remaining). (The per-device
  `usb_msd` reset logs `MSD_RECOVERY_RESET`, the per-device
  Degraded/Recovering/Recovered/fail-closed edges landed above, and the
  `usb_msd` shared-*transport* fault-domain edges landed with IO4.)
- **Device/array health via `sysinfo`** (§16.6): **landed** for the
  kernel-observed volume I/O health surface. A new capability-gated
  `SysinfoQueryId::VOLUME_IO_HEALTH` (id 27, `CAP_SYSINFO_KERNEL`, audited)
  over `IntrospectDomain::VolumeIoHealth` returns one `VolumeIoHealthRecord`
  per fault-aware block-backed volume: its durable id, the serving
  block-service endpoint, its current `MountAvailability`, and the cumulative
  `blkio::BlkHealthCounters` the kernel `BlkClient` folds from every completion
  (per-status outcome tallies + consumer reissue count). The counters are a
  shared `lib/abi` primitive whose status→bucket assignment is one definition
  (`BlkHealthCounters::bucket_index`), so the kernel's lock-free
  `BlkHealthCountersAtomic` and the pure value type cannot diverge; the mount
  registry snapshots them through the `FilesystemService::volume_io_health_snapshot`
  seam and `sysinfod` fronts the query, exposed by `sysinfo storage`. Proven
  host-side end to end (ABI round-trip/fail-closed, the `BlkClient` fold, the
  mount-registry snapshot, the gated/audited/paged broker, and the CLI render).
  Retry/reset *ladder* counts the driver performs on the hardware, and the
  fault-domain **node** identity, are **not** in this record: they need the
  endpoint→hardware-tree-node association and the live serving-driver ladder,
  which are IO4 (fault-domain tree wiring) — this query reports the health the
  kernel block *consumer* observes, a complete and coherent surface on its own.
  Never a `/proc`-style scrape (§16.1).
- **Watchdog tie-in** (`plans/WATCHDOG.md`): a driver process that itself wedges
  is a lockup the watchdog detects and recovers (restart the driver, mark the
  device `Failed`) — closing the loop where the *driver*, not the disk, is the
  problem.

Tests (§7): assert the health-log events for each transition (including the
returning-disk recovery event) and the `sysinfo` health read; a wedged driver
process is detected and recovered.

### Stage IO6 — RAID / mirror / ARXFS composition. **in progress**

**Landed (the RAID1 mirror composition engine):** a RAID volume is a **virtual
block device that composes child block endpoints** through the same fault-aware
`Block` seam every leaf device uses (§2.2 one seam, §27 complete abstraction),
so it nests naturally over the recursive seam and *consumes* the block-layer
health vocabulary (`blkio::BlkStatus`, `DriverError`) rather than re-inventing
it. The first composition is the RAID1 mirror `raid::MirrorArray`
(`drivers/storage/raid`, host-testable `lib`): it is `no_std`,
`forbid(unsafe_code)`, and **allocation-free** — it borrows a caller-owned
member slice, so there is no fixed member ceiling (§24.1) and the growable
member tier lives in the assembling serve process. Its complete behaviour is
proven host-side over a fault-injecting `Block` double:
- **Read = recover + repair.** Reads are served from an in-sync copy in a
  deterministic order; a *per-block* `MediumError` is recovered from a good
  copy and the bad copy is **repaired** in place (opportunistic read-repair),
  and only a *whole-device* fault (`DeviceOffline`/`DeviceFault`, or a member
  returning a request-level error for a request the array already validated)
  drops a copy. A read with no surviving copy fails closed (§5.4).
- **Scrub = proactive verify + repair.** The read-path repair only ever touches
  the copies a read consults *before* the serving one, so a latent media error
  on a copy that is never the read source stays invisible until the copies
  ahead of it are gone — the classic latent-sector data-loss window (§26.5).
  `MirrorArray::begin_scrub`/`scrub_step` close it: a bounded, interruptible
  pass (`scrub_cursor`/`scrubbing`) reads *every* in-sync copy of *every* block
  and repairs a per-block media error from a good copy, dropping only
  whole-device faults — the auto-scrub a mirror exists to provide, chunked so a
  100 TB+ array never scrubs in one sweep or a busy-spin (§26.6, §2.23). A block
  bad on every copy is surfaced as a typed loss but the cursor still advances
  (no loop on the unrepairable block); a failed array (no in-sync copy) fails
  closed without advancing. It deliberately does **not** arbitrate a *content*
  disagreement between two readable copies — a bare mirror has no authority to
  pick the correct one; that is the checksummed FS layer's job (ARXFS) — so its
  remit is latent *media* errors. Scrub buffers are `BufferClass::Sensitive`.
- **Write = fan-out + drop.** Writes fan out to every copy; a copy that fails a
  write is dropped immediately and the write still succeeds as long as one copy
  accepted it, failing closed only when none did. A rebuilding copy receives
  writes to its already-synced region so it never falls behind the source.
- **Degrade, never fail the system.** A faulted copy degrades the array
  (`ArrayHealth::Degraded`) while the survivors keep serving; flush keeps at
  least one durable copy or fails closed. Array health maps onto the shared
  `MountAvailability::{Available,Degraded,Recovering,UnavailableLost}` so a
  serving process surfaces it through the same `sysinfo` mount surface a leaf
  volume uses (IO2/IO5), never a second vocabulary (§2.2).
- **Missing members are first-class (md-style "removed" slots).** A slot the
  array is defined to have but which holds no device is `MemberState::Absent`:
  `assemble` is given the array's *full* member table (one
  `MirrorMember::absent()` per missing copy), counts the absent slot toward
  the member count, and reports `Degraded`, so a mirror short a member never
  masquerades as a smaller optimal array (§26.5). The runtime disk-replacement
  cycle is `remove_member` (pull a faulted disk, slot → `Absent`, returns the
  device) then `add_member` (install a spare into an absent slot → `Resyncing`,
  rebuilt from a survivor), failing closed on a bad index / occupied slot /
  geometry mismatch — full redundancy restored without a reboot (§18.4).
- **Rebuild = bounded, interruptible resync.** A returning copy (via its own
  IO3 grace window) or a physically replaced disk is rebuilt by
  `MirrorArray::resync_step`, which copies the array from an in-sync source a
  caller-sized chunk at a time, so a 100 TB+ rebuild never blocks the system or
  busy-spins (§26.6, §2.23). A rebuilding copy is a read source only once fully
  in sync (`ArrayHealth::Recovering` meanwhile). A faulted copy is
  sticky-but-recoverable (`readd_member`/`replace_member`), so a flapping disk
  never masquerades as a healthy copy yet a genuine return rejoins without a
  reboot (§18.4).
  Design: `docs/src/drivers/raid.md`.

**Landed (the on-disk array metadata + reassembly logic):** the prerequisite
for the autoloaded serve process is the shared, host-tested metadata layer in
`drivers/storage/raid` (§2.2, §27), so an array is *discovered*, never
configured (§18, §16.5). Each member carries a fixed-size, little-endian
`superblock::ArraySuperblock` — a 128-bit `ArrayUuid`, the `RaidLevel`, the
member count, this member's slot, the array geometry, a monotonic
**generation** counter, and a `Time64` last-write stamp (§21) — sealed with a
trailing CRC-32C (`lib/crc32c`, the one first-party checksum, an integrity
check not a security control). `ArraySuperblock::decode` fails closed on every
malformed byte (bad magic, unknown version, checksum mismatch, unknown level,
zero members, out-of-range slot, degenerate geometry, non-canonical timestamp,
§5.4/§26.5); it is total, `forbid(unsafe_code)`, and fuzzed for panic-freedom
(`tests/fuzz_superblock.rs`, registered in `cargo xtask fuzz`, §19.6). The
reassembly verdict is carried into the composition through one shared mapping
`raid::MemberRole::for_slot(SlotDisposition)` (§2.2): a `Present{in_sync:false}`
slot — a copy the generation counter proved is behind — becomes a
`MemberRole::Stale` member, which `MirrorArray::assemble` admits `Resyncing` (a
rebuild target, never a read source) rather than `InSync`, so a copy known to
be out of date can never be served to a reader as if it were current
(§5.4/§26.5 — closing a latent stale-read gap where `assemble` previously
trusted every probeable member as an immediate read source). The reassembly is
the pure, allocation-free `ArrayIdentity`:
`resolve(target_uuid, candidates)` fixes the authoritative array shape and
current generation from the **freshest** matching member (highest generation —
the standard RAID event-count rule, so a survivor that stayed live is the
source of truth), and `verdict_of`/`fill_slots` place each member — in sync,
**stale** (a rebuild target), missing, or refused (foreign array, mis-shaped,
out-of-range slot, or the losing side of a duplicate slot claim) — from **one**
decision, so the per-member verdict and the assembled slot table cannot
diverge (§2.2). The caller owns the candidate slice and slot buffer, so there
is no fixed member ceiling (§24.1). The metadata **write** side — the
counterpart to the read/reassemble side — is the two pure `ArrayIdentity`
primitives `bump_generation` (advance the array event count on a membership
change, saturating at `u64::MAX` rather than wrapping to a value an
already-written member could match) and `member_superblock(slot, updated_at)`
(the on-disk record a *current* member persists at the array's generation,
fail-closed `None` for an out-of-range slot). On a member drop the survivors
re-stamp at the bumped generation while the absent member keeps its lower one,
so it returns a **stale rebuild target** and can never masquerade as current —
closing the stale-read window (§5.4/§26.5, "a disk that missed writes is a disk
that can lie"); promoting a rebuilt member back to current is the same
`member_superblock` write, so the read and write halves share one notion of
"current" and cannot diverge (§2.2). The format is unfrozen and evolved in place
(§2.13). Proven host-side (round-trip; every fail-closed decode incl. pre-1970
/ post-2038 timestamps; resolve/verdict/fill_slots over stale/duplicate/
missing/mismatch/foreign arrangements; the generation bump incl. saturation,
`member_superblock` round-trip/fail-closed, and the end-to-end
absent-member-returns-stale and rebuilt-member-promoted-current journeys). The
discovery-grouping step that precedes per-array assembly is the pure,
alloc-free `raid::distinct_arrays(candidates)` iterator: discovery hands the
assembler a heterogeneous set of decoded-superblock candidates that need not
all belong to one array, and this enumerates the distinct `ArrayUuid`s present
(each once, first-appearance order, no member/array ceiling, §24.1) so the
serve process resolves each array through `ArrayIdentity::resolve` in turn — an
array is *discovered*, never configured (§18, §16.5). Proven host-side (empty,
single-array dedup, interleaved multi-array first-appearance order, and
compose-with-`resolve`).
Design: `docs/src/drivers/raid.md`.

Remaining:
- The autoloaded serve process that reads each discovered device's superblock,
  groups them with `distinct_arrays`, assembles each through `ArrayIdentity`,
  drives `resync_step` off the members' IO3 recovery signals, and publishes the
  composed device as its own block-service node — plus the ARXFS-native multi-device composition that
  consumes the same engine. This rides with the multi-device volume-assembly
  work; the engine and its metadata are the single shared definition both reuse
  (§2.2), proven host-side first exactly as the other FIX-IO primitives landed
  their shared logic before their live wiring.
- **Landed (the RAID0 stripe sibling):** `raid::StripeArray`
  (`drivers/storage/raid`, host-testable `lib`) composes child `Block` members
  as one logical device of their *summed* capacity, round-robining fixed-size
  chunks (`ArraySuperblock::chunk_blocks`, a persisted per-array stripe unit —
  policy, not a global const, §24.1) across the members. It is a sibling of the
  mirror over the same seam (§2.2 parallel implementations), sharing the
  mirror's whole-device-fault classification (`member_faulting`) and
  `ArrayHealth` vocabulary rather than re-inventing them. A stripe has **no
  redundancy** and the engine is honest about it (§5.4, §26.5): `assemble`
  requires every member present and evenly striped (no coming up degraded over
  a gap it cannot serve — it fails closed on an unavailable/mismatched/unaligned
  member), a whole-device fault fails the array closed for good
  (`ArrayHealth::Failed`, sticky — nothing to rebuild from), and a per-block
  media error fails only that request while the still-reachable device keeps
  serving its other stripes; it therefore reports only `Optimal` or `Failed`.
  The on-disk `ArraySuperblock` grew a `RaidLevel::Stripe` + `chunk_blocks`
  (evolved in place, §2.13; level and stripe unit must agree or the record is
  refused `BadStripeChunk`), threaded through `ArrayIdentity`'s shape match and
  reassembly. Allocation-free (borrows a caller-owned member slice, §24.1),
  `forbid(unsafe_code)`, proven host-side over a fault-injecting `Block` double
  (striping-map white-box, cross-stripe gather, every assemble refusal, the
  whole-device-fail-closed and per-block-media-only journeys, all-member flush,
  range validation, buffer-class forwarding). Design: `docs/src/drivers/raid.md`.
- RAID levels beyond the mirror and the stripe (parity: RAID5/6) are further
  sibling compositions over the same seam (§2.2 parallel implementations),
  added when needed.

Tests (§7): the mirror engine is proven host-side in `drivers/storage/raid`
over a fault-injecting `Block` double — two healthy copies assemble optimal; a
bad sector is recovered from a copy and the bad copy repaired; a whole-device
read fault drops a copy and degrades the array; a read with no good copy fails
closed without faulting medium copies; a write error drops a copy while the
write still succeeds; a write no copy accepts fails closed; a returned copy is
rebuilt with **current** data (including degraded-window writes); the rebuild is
incremental (a cursor advances a chunk per step); a write during rebuild reaches
only the already-synced region; a rebuild target that cannot be written drops
back to faulted; a permanently-faulted copy never stops the survivor serving;
assemble/re-add fail closed on empty/mismatched/absent members; a missing
member slot assembles `Absent`, counts toward the width, and reports
`Degraded` (not `Optimal`) while an all-absent set fails closed; `remove_member`
vacates a faulted slot to `Absent` returning the real device (and refuses a
live one), `add_member` rebuilds a spare into an empty slot with current data
(and refuses an occupied slot or bad geometry), and the full remove→add→rebuild
replacement cycle restores redundancy. Scrub is proven
the same way — a clean pass reads *every* copy (unlike a read) and is an
idempotent no-op once complete; a latent bad sector on a non-primary copy the
read path never repairs is found and healed by a scrub; a whole-device fault is
dropped; a block bad on every copy is surfaced yet the cursor still advances; a
two-block scratch scrubs the array a chunk at a time and `begin_scrub` restarts
it; a failed array fails closed without advancing; a ragged/empty scratch is
rejected.

---

## 3. Cross-cutting test floor (§26.7 / §7)

Woven through the stages, and asserted as a combined vertical once IO1–IO3 land:
small discovered RAM (on the order of 1 GiB) with several large volumes mounted,
one of them stalling/faulting — assert bounded resident metadata, no panic, no
busy-spin, the live volumes' throughput unaffected, the faulting device ridden
through its grace window (recovered if it returns, failed closed to *its*
callers only if it does not), and the whole system responsive throughout.

---

## 4. Scope / escalation (§15.7)

This spans `lib/abi` (block seam + status/health enum + fault-domain), the
kernel IPC/timer path (async call post/reap/cancel + per-request deadline +
the `CallReply` wait-set source), every block driver (`usb_msd`, `virtio_blk`,
`emmc2`), `volmgr`, the filesystem drivers, `sysinfo`, and `lib/log`. It is
larger than one change, hence the staging above: each stage lands complete and
green on its own. The IO1 transport shape is decided — the async
submit/complete seam (Stage IO1) — so the downstream stages build on the
ticketed `CallEndpoint` + wait-set event loop and share it with the in-flight
HCD async loop (`plans/USB.md`) rather than a parallel mechanism (§2.2). IO6's
composition **engine** (the `drivers/storage/raid` mirror) is independent of
ARXFS and has landed host-side; only its live serve process and the
ARXFS-native multi-device composition that consumes the same engine remain, and
those ride with the multi-device volume-assembly work.
