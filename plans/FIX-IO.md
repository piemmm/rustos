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
model. IO6 depends on RustFS existing and is gated on it.

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
diverging (§2.2). Wiring a driver's one-shot timer to call `poll` is the
remaining IO4 idle-timer work below.

Remaining:
- `virtio_blk` and `emmc2` are currently consumed **in-kernel** (root-unlock)
  and expose only their `Block` implementation — they are not yet brought up as
  user-space serving processes with their own serve loops. When either is, it
  reuses the shared `blkio::serve_request_recovering` engine (above) rather than
  copying it, so there is nothing to "adopt" separately; the shared engine is
  the single definition (§2.2). Bringing those serve processes up is tracked
  with their user-space driver-process work, not this stage.
- The bounded recovery *escalation* (retry-with-backoff → LUN/device/port
  reset) as an explicit driver action behind the reply-reissuable reporting.
  The background grace-window expiry for a `Recovering` device that receives no
  further request now has its primitive (`BlkHealth::poll` /
  `grace_deadline_ns`, above); only wiring a driver's own one-shot idle timer
  to call it remains (belongs with the driver's idle timers, Stage IO4).
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
  - The driver runs its bounded recovery escalation, each step time-boxed and
    event-driven: retry-with-backoff → LUN reset → device/port reset → (escalate
    to the fault-domain owner for, Stage IO4) hub reset → controller reset. No
    unbounded retries (§2.1); backoff parks on a one-shot timer (§2.23).
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

Remaining:
- **Wiring the tree.** Walk the hardware tree (`hwtree.rs`) to associate each
  block device with its parent bus/hub/controller `FaultDomain`, so a serving
  driver folds `child_status` into each child's completion and a serving/bus
  driver calls `quiesce`/`resume`/`poll` around its own reset. This belongs
  with the user-space bus/serving driver work (it needs the live serve loops and
  timers the host doubles cannot express), like the per-device idle-timer wiring
  in IO3.
- Propagation reuses the existing hotplug path (`hw_emit_node`/`hw_remove_node`,
  `plans/USB.md`, `plans/DEVICES.md`); the device manager gains reaction to
  *degrade/reset/restore* health transitions alongside add/remove.

Tests (§7): the pure `FaultDomain` machine is proven host-side in `lib/abi` —
one owner reset holds the whole subtree reissuable under one window; an owner
returning inside the window recovers the subtree leaving no scar; an owner that
outlasts it fails the subtree closed; a quiet domain expires on the one-shot
time poll; a continuing reset cannot postpone the fail-closed; a failed subtree
is sticky until a demonstrated return. A QEMU vertical of a modelled hub with
several children resetting (asserting one recovery episode across the subtree)
lands with the tree wiring above.

### Stage IO5 — Health observability (audit log + `sysinfo`). **in progress**

**Landed (the mount-availability half):** the `sysinfo` mount table already
distinguishes a live-but-unwell volume — `MountAvailability::{Degraded,
Recovering}` (`lib/abi/sysinfo.rs`) are surfaced by the IO2 consumer overlay
(the mount snapshot's `overlaid_availability`), and `mount`/`df`/`sysmon`
render them with a `[degraded]`/`[recovering]` marker while the vanish states
stay authoritative. The C-ABI view (`include/tairix/tairix_sysinfo.h`)
regenerated through the drift guard.

Remaining deliverables:
- **Health events through `lib/log`** (`plans/SYSLOG.md`) with stable event IDs
  on the hash-chained audit trail (§19.4): every fault, retry, reset (naming the
  fault-domain node), degrade, grace-window entry/expiry, and — importantly —
  every *recovery* ("disk came back"). Security-relevant decisions (driver
  quarantine/restart) stay on the audit log; routine advisories may also use
  `stdinfo` (§20) but never *instead of* the audit log.
- **Device/array health via `sysinfo`** (§16.6) beyond the per-volume mount
  availability already landed above: a capability-gated device/array health
  query (fault-domain node, retry/reset counts) following the bond-member and
  surprise-removed-volume precedents in `sysinfo.rs` — never a `/proc`-style
  scrape (§16.1).
- **Watchdog tie-in** (`plans/WATCHDOG.md`): a driver process that itself wedges
  is a lockup the watchdog detects and recovers (restart the driver, mark the
  device `Failed`) — closing the loop where the *driver*, not the disk, is the
  problem.

Tests (§7): assert the health-log events for each transition (including the
returning-disk recovery event) and the `sysinfo` health read; a wedged driver
process is detected and recovered.

### Stage IO6 — RAID / mirror / RustFS composition. **planned, gated on RustFS**

Deliverables:
- A RAID/mirror/RustFS volume is a **virtual block device that composes child
  block endpoints** through the same fault-aware `Block` seam (§2.2 one seam,
  §27 complete abstraction). It consumes health/status; it does not re-invent
  it.
- A child going `Faulted`/`Offline` **degrades the array, not the system**:
  mirrors/parity serve from surviving members and the array reports `Degraded`
  upward. A returning disk (via the IO3 grace window) triggers **resync/rebuild**
  (bounded, incremental, interruptible per §26.6), and its transition back to
  `Healthy` is logged.
- Multi-layered sets nest naturally because fault domains (IO4) and the block
  seam (IO1) are recursive.

Tests (§7): a composed mirror over fault-injecting children — one child faults
and recovers inside its grace window (array degrades then resyncs), one child
expires (array stays degraded, system unaffected).

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
HCD async loop (`plans/USB.md`) rather than a parallel mechanism (§2.2). IO6 is
gated on RustFS existing; IO1–IO5 do not depend on it and land first.
