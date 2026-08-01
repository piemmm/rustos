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
per-class deadline/retry/grace/queue-depth policy, and a consumer *discovers*
which class to apply rather than assuming one: the driver that binds the
hardware declares it (`Block::device_class`; `Removable` for a USB LUN and the
SD host, `Virtual` for virtio-blk, default `Virtual` = the bounded
unclassified envelope), it travels in the geometry completion
(`BlkCompletion::class`, decoded fail-safe to `Virtual` on an unrecognised
word), and both consumers adopt it at connect for every later request. The
class is patience policy, not authority, so an untrusted driver cannot buy
itself more than the widest class budget. Every wrapping layer forwards its
inner device's class (partition window, block cache, `SharedBlock`, the
retained-writes journal, both remote clients) and a composition folds its live
members with `BlkDeviceClass::most_patient` (the six RAID engines +
`RaidArray`, through the shared `aggregate_device_class`), so the real
hardware's envelope survives every layer. Consumers rewritten in place: the
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
  deadline, retry count, grace window, and queue depth are derived from the
  device's *declared* class (a rotational SATA disk ≠ an NVMe namespace) and
  fail closed under pressure (§26.1). The class → budget mapping lives in one
  place both the serving driver and the consumer read, never a copied literal
  (§2.2), and the class itself is declared by the driver that binds the
  hardware and carried to the consumer in the geometry completion, so no layer
  guesses it.

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

### Stage IO2 — Consumer-side isolation (volmgr + filesystems). **done**

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

- **Proven on the live kernel, not only host-side.** The QEMU vertical
  `tests/integration/blkio_fault_qemu_aarch64` (+ its EL0 fixture
  `blkio_fault_program`) closes the gap the host doubles cannot express,
  because the per-request deadline, the `CallReply` wait-set source, and the
  ticket lifecycle are kernel machinery. Its chassis installs the production
  `KernelDispatchHook` and spawns one fixture holding only `CAP_IPC_ENDPOINT`;
  its drive loop performs the timed-wake sweep a production timer tick would,
  which is what lets an elapsed deadline wake a parked reaper. The fixture
  stands up two block-service endpoints — a healthy one served through the
  shared `blkio::serve_request_recovering` engine over a fault-injecting
  device and consumed through the production `RemoteBlock`, and a wedged one
  never serviced — and asserts, through real traps: a transient blip is ridden
  out inside the shared per-class reissue budget and returns correct data (the
  device confirming it really injected exactly `IoBudget::max_retries` faults,
  so the ride-out cannot pass vacuously); a blip that outlasts the budget
  fails closed as the typed transient class; a bad sector keeps its own
  medium-error class; a request outstanding to the wedged device neither
  stalls the healthy device (sixteen interleaved transfers complete, each
  verifying its data) nor completes early, its elapsed deadline wakes the
  parked reaper exactly like a real completion, and the claim then reaps
  `TimedOut` no earlier than the deadline; and a per-ticket `call_cancel`
  withdraws an outstanding request while a foreign ticket cancels nothing.
  Each failure site carries its own exit code, folded into the QEMU finisher.
  Both halves live in one address space, so the vertical proves the
  *transport's* head-of-line freedom rather than a second process boundary
  (per-driver process isolation is the §4 property the driver-spawn verticals
  already prove).

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
  with their user-space driver-process work, not this stage. Even in the
  in-kernel form, each driver's `Block` already reports the honest per-request
  health class (root cause #3): `virtio_blk` decodes the `virtio_blk_req` status
  byte through one shared `status_to_result` — `VIRTIO_BLK_S_IOERR` → a
  per-request `DriverError::MediumError` (recovered/repaired by a consumer, not a
  whole-device drop), `VIRTIO_BLK_S_UNSUPP` → `Unsupported`, any undefined status
  → a fail-closed `DeviceFault` — so no consumer (a RAID member, the kernel block
  client) drops a whole device over a single bad sector, before the serve-loop
  wrapping even exists.
- The consumer marking the affected volume degraded landed in IO2 above (the
  kernel `BlkClient` overlay surfaced through `MountAvailability::{Degraded,
  Recovering}`); the remaining observability is the audit-log health trail
  (Stage IO5).
- The **return-inside-window** leg is proven on the live kernel by the IO2
  vertical above (`tests/integration/blkio_fault_qemu_aarch64`): a device that
  stalls transiently is held `Recovering` by the shared engine, answered
  reissuably, reissued by the production consumer within the shared per-class
  budget, and returns correct data once it recovers — with the fault injection
  itself asserted, so the ride-out cannot pass vacuously. What remains is the
  **grace-expiry** leg (fault → window elapses → `Faulted` → fail closed →
  D4 retention). It cannot ride on that vertical: the narrowest real class
  window is eight seconds of wall clock, and shortening one for a test would
  make the vertical assert a policy the system does not ship. It needs a
  vertical whose guest time source it can advance, or a class whose window a
  real device legitimately declares that short.

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
- **Watchdog tie-in** (`plans/WATCHDOG.md`, `plans/NEW-SERVICEMANAGER.md`): a
  driver process that itself wedges is a lockup the supervisor detects and
  recovers (restart the driver, so the device recovers rather than staying
  failed) — closing the loop where the *driver*, not the disk, is the problem.
  This is **not** a FIX-IO-local mechanism: bounded, backed-off, crash-loop-
  guarded restart already lives in the service manager (SVC-7), so re-inventing
  it here would duplicate it (§2.2). The **liveness-watchdog engine core** that
  turns a *wedged* (present-but-unresponsive) supervised process into that same
  restart path landed in the service manager as SVC-8: a per-service
  `WatchdogSec`-equivalent interval in the signed unit metadata
  (`lib/abi::ServiceUnit::watchdog`), and the `Init` engine's
  `arm_watchdogs`/`heartbeat`/`watchdog_deadline`/`expire_watchdog` (a missed
  heartbeat force-kills the wedged process and drives the existing
  `RestartPolicy`/backoff/crash-loop budget), proven host-side. The **live
  wiring** — a user-space block-driver serve loop renewing its heartbeat to its
  manager as it makes progress, so a wedged serve loop is force-restarted —
  rides with the SVC-5/SVC-8 control transport (the heartbeat-renewal path and
  the reactor that arms the real one-shot off `watchdog_deadline`), exactly as
  the other FIX-IO primitives landed their shared logic before their live
  wiring. A driver process that keeps wedging past the crash-loop budget is left
  down, and its device fails closed to its consumers through the existing IO1
  per-request deadline / IO3 health machinery.

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
- **Composed device health.** Because each array is itself a `Block`, all four
  compositions override `Block::device_health` to aggregate their live members'
  `SMART`/`NVMe` telemetry through one shared `raid::health::
  aggregate_device_health` (§2.2) rather than inherit the trait default and hide
  every member's health from the FS scrub scheduler (§26.5, ARXFS §11):
  independent integrity counters sum (saturating), shared conditions take the
  worst member, faulted/absent slots and members with no telemetry contribute
  nothing, and the array reports `Unavailable` only when no live member exposes
  any. Proven host-side per array (aggregate, faulted/absent exclusion,
  resyncing inclusion, all-unavailable, errored-member skip).
  Design: `docs/src/drivers/raid.md`.

**Landed (the on-disk array metadata + reassembly logic):** the prerequisite
for the autoloaded serve process is the shared, host-tested metadata layer in
the `lib/raidmeta` crate (§2.2, §27), so an array is *discovered*, never
configured (§18, §16.5). It lives in `lib/*`, not the driver, because a second
consumer — the storage-discovery probe (`lib/fsprobe`/volmgr) — reads the same
definition to refuse mounting a bare member (below), which a `drivers/*` crate
could not without a `drivers/*`→`drivers/*` edge (§17.4). Each member carries a
fixed-size, little-endian `ArraySuperblock` — a 128-bit `ArrayUuid`, the
`RaidLevel`, the
member count, this member's slot, the array geometry, a monotonic
**generation** counter, and a `Time64` last-write stamp (§21) — sealed with a
trailing CRC-32C (`lib/crc32c`, the one first-party checksum, an integrity
check not a security control). `ArraySuperblock::decode` fails closed on every
malformed byte (bad magic, unknown version, checksum mismatch, unknown level,
zero members, a member count its level cannot be composed from — RAID5 < 3,
RAID6 < 4 or > 257 slots, the shared `RaidLevel::min_members`/`max_members` the
composition engines also read (the level's *usable capacity* is the same shared
source — `RaidLevel::data_members`/`logical_block_count`, from which the
concatenating engines size the composed geometry so it cannot drift, §2.2) —
out-of-range slot, degenerate geometry,
non-canonical timestamp, §5.4/§26.5); it is total, `forbid(unsafe_code)`, and fuzzed for panic-freedom
(`lib/raidmeta/tests/fuzz_superblock.rs`, registered in `cargo xtask fuzz`, §19.6). The
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

**Landed (the composed-device dispatch — `RaidArray`):** once an array is
*discovered* and its `RaidLevel` resolved, a serve process presents exactly one
logical `Block` device regardless of level. That single composed-device
abstraction is the shared `raid::RaidArray` enum (§27, modelled on Linux md's
per-personality dispatch): an allocation-free dispatch layer over the four
engines that forwards the whole `Block` I/O path *and* the level-agnostic
surface — observation (`level`/`health`/`member_count`/`array_geometry`/
`member_state`/`needs_resync`/`scrubbing`/`scrub_cursor`), self-maintenance
(`begin_scrub`/`scrub_step`/`resync_step`, taking one scratch buffer that both
sizes the bounded chunk, §26.6, and stages the mirror while the parity levels
size their budget from it), and the hot-swap reconfiguration
(`readd`/`remove`/`add`/`replace_member`) — mapped onto one shared `RaidError`.
So neither the autoloaded serve process nor the ARXFS-native composition
re-derives the level → engine mapping (§2.2). It adds **no** policy of its own;
the no-redundancy RAID0 stripe fails every redundancy-only op closed with
`RaidError::NotRedundant` (the level check winning over scratch validation),
exactly as the stripe engine reports only `Optimal`/`Failed`. Proven host-side
(per-level I/O round-trip incl. through a `&mut dyn Block`, observation, the
mirror and block-budget parity maintenance dispatch, the stripe `NotRedundant`
refusals, `BadScratch`, and the `RaidError` `From` mapping incl. its defensive
catch). Design: `docs/src/drivers/raid.md`.

**Landed (discovery-side member recognition — a bare member is never mounted):**
the on-disk metadata format and reassembly were hoisted from the driver into
the shared `lib/raidmeta` crate so the composition driver *and* the
storage-discovery probe read one definition of what a member is (§2.2) without
a `drivers/*`→`drivers/*` edge (§17.4). `lib/fsprobe` gained
`probe_raid_member` (a validated `ArraySuperblock::decode` at an extent's
block 0 — magic, version, CRC, and every bounds check — returning the array
UUID, fail-closed), and `drivers/storage/volmgr`'s probe plan now recognises a
RAID array member — whole-device *or* per-partition — **before** any filesystem
signature and refuses to attach it (`PlanSummary::raid_members`, a distinct
`VOLMGR_RAID_MEMBER` audit event, never counted as blank). This closes a
latent data-integrity hole: a bare mirror copy holds a full filesystem at the
array's data offset, so mounting one raw copy read-write would silently diverge
the array or serve stale data from a member that missed writes (§26.5). Proven
host-side (fsprobe recognises and round-trips a member, fails closed on
blank/filesystem/short/corrupt input, and fuzzes the new decoder; volmgr skips
a whole-device and a partition member while still attaching sibling
filesystems). The member superblock's block-0 placement is the contract this
recognition reads; how the array's data is laid out relative to it is fixed by
the assembling serve process below.

**Landed (the reassembly→member bridge):** the pure link between the metadata
layer (which resolves a discovered array to a `SlotDisposition` per slot) and
the composition engines (which each borrow a caller-owned member buffer) is the
shared, host-tested `raid::fill_members` + `AssembleMember` bridge
(`drivers/storage/raid`, §2.2, §27). Every consumer that assembles a
*discovered* array — the autoloaded serve process and the ARXFS-native
multi-device composition alike — would otherwise hand-roll the same placement
loop, and a slip is a data-integrity fault, not a cosmetic one: admitting a
slot the generation counter proved stale (`in_sync == false`) as a trusted read
source, or losing a copy when the buffer width and the slot table disagree
(§5.4, §26.5 "a disk that missed writes is a disk that can lie"). `fill_members`
places each slot through the single role authority `MemberRole::for_slot`, so a
stale copy joins `Stale` (a rebuild target admitted `Resyncing`, never a read
source), a missing slot becomes an absent member so the array knows its true
width, and a present slot whose device the caller cannot supply, or a member
buffer the wrong width, fails closed (`AssembleError::{MissingDevice,
WidthMismatch}`) rather than composing a partial array. It is defined over the
redundant member types that carry the current/stale/absent vocabulary
(`MirrorMember` — shared by the mirror and RAID10 — `ParityMember`,
`DualParityMember`, `TripleParityMember`); the no-redundancy RAID0 stripe is
deliberately excluded (its `assemble` fails closed on a gap). `no_std`,
`forbid(unsafe_code)`, allocation-free, proven host-side (current/stale/absent
placement, cross-type uniformity, and the width-mismatch/missing-device
fail-closed cases). Design: `docs/src/drivers/raid.md`.

**Landed (the maintenance scheduler — *when* an array heals itself):** exposing
a self-healing surface is not the same as driving it. `RaidArray` offers
`readd_member`/`resync_step`/`begin_scrub`/`scrub_step`, but an array only heals
itself if something decides, turn by turn, which to do next — and when to do
none so the foreground workload keeps the array (§26.1, §26.2, §2.16). Both
named consumers (the autoloaded serve process, the ARXFS-native composition)
need that decision, and a slip in it is a data-integrity or availability fault,
not a cosmetic one: a rebuild never started leaves the array degraded until the
*next* fault loses data (§26.5); an unpaced one starves the workload; a blind
re-probe loop is the busy-wait §2.23 forbids while never re-probing strands a
disk that came back (§18.4). `raid::ArrayMaintenance` is that one decision
(§2.2, §27) — pure, allocation-free, and event-timed (it holds no clock and
never spins: the caller supplies the monotonic reading, and `wait_deadline_ns`
gives the absolute one-shot deadline the serve loop parks on, the same idiom
`BlkHealth::grace_deadline_ns` uses; the per-member re-add records live in a
caller-owned slice, so a wide array has no fixed ceiling, §24.1). Its priority
restores redundancy before verifying it: re-admit a faulted member whose backoff
has elapsed, then advance a rebuild, then advance or start a proactive scrub —
the scrub **only** on a fully `Optimal` array, so an array that degrades
mid-pass pauses at its cursor and resumes once redundancy is back rather than
spending I/O it cannot repair from. Maintenance yields to the workload by a
**duty share** (a chunk taking `d` holds the next off `d × (100 − duty) / duty`
while foreground traffic is present, full speed when idle), which is chunk-size
independent and so needs no per-device retuning, unlike Linux md's global
KB/s `speed_limit_*`; an out-of-range share is clamped so a mis-set policy can
neither stall maintenance nor divide by zero, and a failed chunk backs off by
the class's grace window instead of hammering unwell hardware.
`MaintenancePolicy::for_class` derives the defaults from the array's
*discovered* class (the members' `BlkDeviceClass` fold), never a frozen scalar
(§24.2): the first re-add delay **is** that class's `IoBudget::grace_ns` — one
definition, so a member is never re-probed before its own driver would have
given up on it — doubling to a 32× ceiling, and a demonstrated return
(`note_member_returned`, the IO3/IO4 recovery signal) collapses an escalated
wait back to that base delay and no further, so neither a flapping member nor a
repeating signal becomes a re-probe storm. The scrub cadence and the busy window
are properties of the accepted risk and of the workload rather than of the
hardware, so they are one documented default per array, settable through the
policy's public fields; `ArrayMaintenance::new` takes the elapsed time since the
last completed pass from the caller's persisted record, and `u64::MAX` (no
record) makes the first pass due at once — an array whose verification history
is unknown is verified, not assumed clean (§5.4, §26.5). Deliberately out of
scope: it never installs or removes a device (an `Absent` slot awaits an
operator spare), drives nothing on a `Failed` array (with no in-sync member
there is nothing to rebuild from, and admitting a copy as current would serve
data the array cannot vouch for — recovery there is a superblock re-resolution,
an assembly decision), and drives nothing on a non-redundant stripe. Whether a
level has redundancy at all is now the single shared `RaidLevel::is_redundant`
in `lib/raidmeta`, which the `RaidArray` dispatch also refuses its
redundancy-only operations with, so the dispatch and the scheduler cannot
disagree about which arrays can heal themselves (§2.2). Proven host-side (the
class-derived policy; the width-mismatch fail-closed; the priority order; the
degraded/absent/failed/stripe leave-alone cases; the backoff escalation, ceiling
and signal floor; deterministic slot choice; idle/busy/clamped pacing and the
failed-chunk backoff; the scrub start, completion re-arm, refusal deferral, and
the pause-and-resume across a degrade; and that an idle deadline is never in the
past, so parking on it is a wait and not a spin). Design:
`docs/src/drivers/raid.md`.

**Landed (durable maintenance progress — a pass survives a restart):** a scrub
and a rebuild each advance a cursor one bounded chunk at a time, so on a 100 TB+
array a full pass runs for **hours or days** — longer than the interval between
reboots on a real machine. Held only in memory the cursor is lost on every
restart, so an array rebooted often enough would never finish a rebuild and
might never be verified at all: a latent, unbounded data-integrity hole exactly
where redundancy should protect the most data (§26.5, §26.6). The position is
now durable, through one definition both halves share (§2.2):
- `raidmeta::ArrayProgress` is the resumable position (scrub cursor + rebuild
  cursor, `None` when not running) — the same value the engines report and
  accept and the on-disk record carries. Every engine and the `RaidArray`
  dispatch gained `progress()` / `restore_progress()`; a stripe reports the idle
  position (an observation, answered for every level) and refuses the restore
  `NotRedundant` like every other redundancy-only operation.
- **The reported rebuild cursor is the least advanced member's.** Members
  rebuild concurrently at different cursors and one record carries one position;
  resuming from the least advanced re-copies blocks a further-ahead member
  already holds (idempotent), whereas the furthest ahead would leave another
  member's outstanding blocks never copied while the array counted it rebuilt.
- **A cursor outside the array is refused, never clamped**
  (`CursorOutOfRange`, its own class through every engine → `RaidError`
  mapping). Adopted as a rebuild position it would declare a member fully
  copied without its tail ever being written, leaving stale data trusted as a
  current read source (§5.4, §26.5). A restored cursor is planted only on
  members actually rebuilding, so it can never un-sync a current copy.
- `raidmeta::MaintenanceRecord` is the on-disk form each member carries in the
  block after its superblock (`MAINTENANCE_BLOCK`; a member's data begins at
  the shared `RESERVED_METADATA_BLOCKS`). It is a *separate* record in a
  *separate* block deliberately: the superblock changes only with the array's
  shape while progress is checkpointed continuously, so a torn write of a
  routine checkpoint can never damage the metadata assembly depends on. It
  carries the array UUID + generation, a checkpoint sequence
  (`is_fresher_than` picks the freshest of the members' copies), the cursors,
  and the `Time64` instant of the last **complete** pass (the value
  `ArrayMaintenance::new` is seeded with, which deliberately survives a
  membership change — verifying the array is a property of the data, not the
  member set). Sealed with CRC-32C, canonically encoded (a field the flags
  declare absent must be zero), and fail-closed on every malformed byte.
  Every way of losing or doubting it degrades toward *more* verification:
  undecodable → no position; foreign UUID → ignored; earlier generation →
  cursors dropped (a member joined/left, so a resumed cursor could skip data
  the new member never received) while the completion stamp stands; a
  completion stamp *ahead* of the wall clock (unset/stepped clock, or forged) →
  "unknown", so a pass is due at once rather than suppressed indefinitely. A
  hostile or failing disk therefore cannot use the record to make an array skip
  work. Fuzzed for panic-freedom including a resealed-corruption adversary
  (§19.6). `Time64` gained the instant-difference and span→nanoseconds helpers
  this needs, completing the pair with its existing offset-by-a-span
  arithmetic.
- **Defect fixed in the same change (§2.18):** `ArrayMaintenance` tracked only a
  scrub *it* had started, so an array handed over mid-pass (exactly the restored
  case) completed that pass without re-arming the period — and since such an
  array's history reads as overdue, it would restart the pass immediately and
  verify itself back-to-back forever, spending I/O the workload should have. It
  now adopts the array's live scrub state at construction; regression test
  `a_pass_resumed_from_the_records_position_still_rearms_the_period` fails
  before the fix and passes after.

Remaining:
- The autoloaded serve process that reads each discovered device's superblock,
  groups them with `distinct_arrays`, assembles each through `ArrayIdentity`,
  populates each engine's member buffer through the shared `raid::fill_members`
  bridge (above), wraps it in the shared `RaidArray` dispatch (above), restores
  the persisted position through `MaintenanceRecord::progress_for` +
  `RaidArray::restore_progress` and checkpoints `RaidArray::progress()` back to
  its members as the array works, turns the shared `raid::ArrayMaintenance`
  decisions into real transfers against its members (driving `note_foreground`
  from its serve path and `note_member_returned` from the IO3/IO4 recovery
  signals, and arming its wait from `wait_deadline_ns`), and publishes the
  composed device as its own block-service node — plus the ARXFS-native
  multi-device composition that consumes the same engine, dispatch, scheduler,
  and record. This rides with the multi-device volume-assembly work; the engine,
  its metadata, the `fill_members` reassembly bridge, the `RaidArray` dispatch,
  the maintenance policy, and the durable progress record are the single shared
  definition all consumers reuse (§2.2), proven host-side first exactly as the
  other FIX-IO primitives landed their shared logic before their live wiring.
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
- **Landed (the RAID5 distributed-parity sibling):** `raid::ParityArray`
  (`drivers/storage/raid`, host-testable `lib`) composes `member_count - 1`
  members' worth of capacity as one logical device with single-fault
  redundancy — the capacity-plus-redundancy sibling of the mirror and the
  stripe over the same seam (§2.2 parallel implementations), reusing their
  `MemberState`/`MemberRole`/`ArrayHealth` vocabulary and `member_faulting`
  classification rather than re-inventing them. It stripes fixed-size chunks
  (`ArraySuperblock::chunk_blocks`) with a left-symmetric rotating parity slot,
  so no member is a parity bottleneck. Its complete behaviour is proven
  host-side over a fault-injecting `Block` double: healthy reads go direct while
  a lost member's chunk is reconstructed (XOR of the survivors) and a per-block
  media error is reconstructed **and repaired**; writes update parity by
  read-modify-write, or recompute it from the surviving data members when the
  data/parity member is lost (a degraded write keeps the missing data
  reconstructable); a proactive `begin_scrub`/`scrub_step` pass heals latent
  media errors chunked so a 100 TB+ array never scrubs in one sweep (§26.6,
  §2.23); one lost member degrades the array while two fail it closed (§5.4,
  §26.5, never fabricating unreconstructable data); a returning or replaced
  member is rebuilt by a bounded, interruptible `resync_step`, and the
  `remove_member`/`add_member`/`replace_member` disk-replacement cycle restores
  redundancy without a reboot (§18.4). It is `no_std`, `forbid(unsafe_code)`,
  and allocation-free — it borrows a caller-owned member slice (no fixed member
  ceiling, §24.1) plus a caller-owned scratch buffer for parity/reconstruction.
  The on-disk `ArraySuperblock` grew a `RaidLevel::Parity` (evolved in place,
  §2.13; level and stripe unit must agree or the record is refused
  `BadStripeChunk`), threaded through `ArrayIdentity`'s shape match. Design:
  `docs/src/drivers/raid.md`.
- **Landed (the RAID6 double-parity sibling):** `raid::DualParityArray`
  (`drivers/storage/raid`, host-testable `lib`) composes `member_count - 2`
  members' worth of capacity as one logical device with **double-fault**
  redundancy — the sibling of the mirror, the stripe, and single parity over the
  same seam (§2.2 parallel implementations), reusing their
  `MemberState`/`MemberRole`/`ArrayHealth` vocabulary and `member_faulting`
  classification. Each stripe reserves *two* rotating chunks: a P (XOR) syndrome
  and a Q (Reed-Solomon `Q = Σ gᵏ·Dₖ`) syndrome over GF(2^8) — the first-party
  `raid::gf256` field (generator `{02}`, polynomial `0x11d`, the Linux-RAID6
  field), with `mul`/`inv`/`gpow` proven host-side (identity/commutativity/
  associativity/distributivity, every non-zero inverse, and the generator's
  full period 255). Its complete behaviour is proven host-side over a
  fault-injecting `Block` double: a single lost chunk is reconstructed from P and
  *any two* lost chunks are solved from the two independent syndromes (through Q
  when P is also lost, through P when Q is also lost, or the 2×2 system for two
  lost data chunks — every distinct loss pair, incl. a six-member array
  exercising higher Q coefficients); a per-block media error is reconstructed
  **and repaired**; writes update P and Q by read-modify-write or recompute both
  from the survivors on a degraded write; `begin_scrub`/`scrub_step` heals a
  latent media error on a syndrome the read path never touches (proven by then
  losing both data members of that stripe); one or two losses degrade the array
  while a *third* fails it closed (§5.4, §26.5, never fabricating
  unreconstructable data); a returning/replaced member rebuilds by a bounded
  `resync_step`, and the `remove_member`/`add_member`/`replace_member` cycle
  restores redundancy without a reboot (§18.4). It is `no_std`,
  `forbid(unsafe_code)`, and allocation-free — it borrows a caller-owned member
  slice (no fixed member ceiling, §24.1) plus a caller-owned scratch buffer of
  at least `SCRATCH_BLOCKS` logical blocks for the syndromes and the two-erasure
  solver. The on-disk `ArraySuperblock` grew a `RaidLevel::DualParity` (evolved
  in place, §2.13; a striped level, so it must carry a non-zero stripe unit or
  the record is refused `BadStripeChunk`), threaded through `ArrayIdentity`'s
  shape match. `assemble` needs ≥ 4 members and fails closed above 255 data
  members (`TooManyMembers`), where the GF(2^8) Q coefficients would collide.
  Design: `docs/src/drivers/raid.md`.
- **Landed (the RAID-TP triple-parity sibling):** `raid::TripleParityArray`
  (`drivers/storage/raid`, host-testable `lib`) composes `member_count - 3`
  members' worth of capacity as one logical device with **triple-fault**
  redundancy — the sibling of the mirror, the stripe, and single/double parity
  over the same seam (§2.2 parallel implementations), reusing their
  `MemberState`/`MemberRole`/`ArrayHealth` vocabulary and `member_faulting`
  classification. Each stripe reserves *three* rotating chunks — a P (XOR)
  syndrome, a Q (`Σ gᵏ·Dₖ`) syndrome, and an R (`Σ g²ᵏ·Dₖ`) syndrome over the
  first-party `raid::gf256` field (`g²ᵏ` via the shared `gf256::gpow2`, one
  definition with the Q coefficients, §2.2). Its complete behaviour is proven
  host-side over a fault-injecting `Block` double: **any** one, two, or three
  lost chunks in a stripe are solved from the three syndromes by a per-stripe
  GF(2^8) Vandermonde matrix inverse (the coefficient rows `(1, gᵏ, g²ᵏ)` over
  distinct nodes, always invertible for ≤3 unknowns) applied byte-wise; a
  per-block media error is reconstructed **and repaired**; writes update P/Q/R
  by read-modify-write or recompute all three from the survivors on a degraded
  write; a proactive `begin_scrub`/`scrub_step` pass heals latent media errors
  chunked so a 100 TB+ array never scrubs in one sweep (§26.6, §2.23); one, two,
  or three losses degrade the array while a *fourth* fails it closed (§5.4,
  §26.5, never fabricating unreconstructable data); a returning/replaced member
  rebuilds by a bounded, interruptible `resync_step`, and the
  `remove_member`/`add_member`/`replace_member` disk-replacement cycle restores
  redundancy without a reboot (§18.4). It is `no_std`, `forbid(unsafe_code)`,
  and allocation-free — it borrows a caller-owned member slice (no fixed member
  ceiling, §24.1) plus a caller-owned scratch buffer of at least
  `SCRATCH_BLOCKS` logical blocks. The on-disk `ArraySuperblock` grew a
  `RaidLevel::TripleParity` (evolved in place, §2.13; a striped level, so it
  must carry a non-zero stripe unit or the record is refused `BadStripeChunk`),
  threaded through `ArrayIdentity`'s shape match, with the shared 255-data-member
  GF(2^8) ceiling generalised to `MAX_PARITY_DATA_MEMBERS` (one definition for
  RAID6 and RAID-TP, §2.2). `RaidArray` dispatch gained the `TripleParity` arm.
  Design: `docs/src/drivers/raid.md`.
- **Landed (the RAID10 stripe-of-mirrors sibling):** `raid::Raid10Array`
  (`drivers/storage/raid`, host-testable `lib`) composes an even number of
  members (≥ 4) into two-copy mirror pairs and stripes fixed-size chunks
  (`ArraySuperblock::chunk_blocks`) across the pairs — the capacity-*and*-
  redundancy sibling of the mirror and the stripe over the same seam (§2.2
  parallel implementations). It is a genuine *composition*, not a
  re-implementation (§2.2): the RAID0 striping map (`StripeArray::locate`,
  hoisted to a shared `pub(crate)` helper) places each chunk on its pair, and
  each pair is driven through the one `MirrorArray` engine via an
  allocation-free transient `MirrorArray::from_prepared` view, so RAID10
  inherits the mirror's recover/read-repair/write-fan-out/scrub/rebuild
  behaviour and adds only the pairing and the aggregation of per-pair health
  into array health. It presents half its members' capacity, survives any
  member fault — and several at once — while no pair loses both copies
  (`ArrayHealth::Degraded`/`Recovering`), and fails a lost pair's region closed
  (`ArrayHealth::Failed`, §5.4) while the other pairs keep serving (head-of-line
  freedom, §26.1). Proven host-side over a fault-injecting `Block` double
  (assemble round-trip and every malformed-table/odd/too-few/geometry/unaligned
  refusal, media-error recover+repair, whole-device degrade, both-copies
  fail-closed-with-sibling-serving, write fan-out+drop, fully-absent-pair,
  replace+rebuild-current-data, remove→add replacement cycle, out-of-range
  member ops, latent-error scrub heal, failed-array scrub fail-closed,
  device-health aggregation, request validation, buffer-class threading). The
  on-disk `ArraySuperblock` grew a `RaidLevel::Raid10` (evolved in place, §2.13;
  a striped level, so a non-zero stripe unit is required, and its member count
  must be even and ≥ 4 — `data_members` is the single composability oracle the
  `decode` boundary reuses so an odd count fails closed identically). `RaidArray`
  dispatch gained the `Raid10` arm. Design: `docs/src/drivers/raid.md`.
- RAID levels beyond RAID10 are further sibling compositions over the
  same seam (§2.2 parallel implementations), added when needed.

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
rejected. Durable progress is proven the same way, per engine and through the
dispatch: a rebuild and a scrub each resume where a restart left them (taking
exactly the remaining chunks, and the resumed rebuild leaving current data on
the rebuilt copy); the reported rebuild cursor is the least advanced of two
copies rebuilding at different positions; a cursor past the end is refused with
its own class while the last real block is accepted; a restored cursor never
touches a current member; a lost record simply starts the passes over; and the
record itself round-trips, refuses every malformed/foreign/superseded form, and
is fuzzed.

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
