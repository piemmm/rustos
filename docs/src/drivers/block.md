# Block drivers

Block drivers expose a fixed-size logical-block array to higher layers
(filesystems, swap, dump). They implement
[`tairix_abi::driver::block::Block`](../abi/driver_traits.md) and are
loaded as user-space drivers unless their manifest declares
`kind = "in-kernel"` (in which case they require `CAP_DRV_KERNEL`).

## Class trait

`Block` exposes these core method families:

| Method                         | Purpose                                       | Capability gate                |
|--------------------------------|-----------------------------------------------|--------------------------------|
| `geometry`                     | report `BlockGeometry { block_size, block_count }` | `DriverHandle` ownership |
| `device_class`                 | report the `BlkDeviceClass` the I/O budget derives from | `DriverHandle` ownership |
| `read_blocks` / `write_blocks` | bulk transfer (multiple of `block_size`)      | `DriverHandle` ownership       |
| `read_blocks_with_class` / `write_blocks_with_class` | classed transfer (see below) | `DriverHandle` ownership |

All methods return `Result<_, DriverError>`. Per `AGENTS.md` §2.9 the
class trait never panics — buffer length errors map to
`DriverError::BufferTooSmall` and out-of-range LBAs map to
`DriverError::LengthOutOfRange`.

Device-reported outcomes carry a **health axis** rather than collapsing to a
single fault. A block-service completion leads with a `blkio::BlkStatus`
word, and every consumer of a served device — the kernel-side `BlkClient`
and the volume manager's probe alike — classifies it through the one shared
`DriverError::from_errno` mapping (`lib/abi`), never a per-consumer copy
(`AGENTS.md` §2.2). The health classes stay distinct so the filesystem layer
can act on them: a permanent bad sector is `DriverError::MediumError`, a
present-but-unresponsive or surprise-removed device is
`DriverError::DeviceOffline`, a transient stall or a device/hub reset is a
reissuable `DriverError::Busy`, and a timed-out or vanished endpoint (and any
unclassified failure) fails closed as `DriverError::DeviceFault`. Because the
mapping is per-consumer-agnostic, a fault on one device surfaces only to that
device's callers while every other mount keeps running (`plans/FIX-IO.md`
IO1/IO2).

## The shared data window has exactly one user at a time

A served block device is reached over **two** things: a call endpoint that
carries the fixed-size `BlkRequest`/`BlkCompletion` frames, and a **shared
data window** that the payload bytes themselves are staged in. The request
frame names an offset and a length; the driver writes the read bytes into
that window, or reads the write bytes out of it. The window is a property of
the *device*, not of a consumer: one node, one window.

That makes the window a **critical section**. Two transfers in flight on one
device would each stage bytes in the same buffer, so one would overwrite the
other's payload and a reader would be handed bytes belonging to a different
extent — silent corruption rather than a fault, and exactly the kind of defect
a checksum-less filesystem would never notice. The rule is therefore absolute:

> A device's data window has one user at a time, and every consumer of that
> device drives it through one client.

It is upheld structurally, never by convention:

- **In the kernel**, the runtime volume service connects a device **once per
  block-service endpoint** and shares that one client behind
  `SharedBlock`'s sleeping lock, so every volume on a disk — each partition's
  mount, and the detach-time device-cache flush — serialises whole operations
  onto it. A mount holds an *owned* window (`OwnedBlockWindow`) because the
  filesystem driver long outlives the attach call that opened it; the last
  window dropped closes the client and releases its window hold. Sharing the
  client also gives a disk **one** health fold and one set of I/O counters
  rather than a divergent copy per mount, which is the honest reading anyway:
  health is a property of the device.
- **Across processes**, the volume manager and the kernel's mounts are ordered
  rather than interleaved. The manager probes the whole device, **drops** its
  transport, and only then asks the kernel to attach the volumes it found.
  Dropping the client consumes the window borrow, so it is the compiler — not
  a comment — that stops a later probe read from racing the mounts it just
  created.

A serving driver correspondingly resolves the window a request names and may
refuse a request naming a window it was never granted (fail closed); it never
assumes the requester is alone.

## The declared device class

How patient a consumer should be with a device is a property of the *device*: a
spinning disk may legitimately take tens of seconds to spin up or finish an
internal reset, while a paravirtual device that has not answered in seconds is
wedged rather than busy. A consumer that assumed one envelope for every device
would either fail a slow-but-healthy disk early or let a wedged fast device
stall its callers far longer than policy intends, so the one component that
knows what the hardware is — the driver that binds it — declares it:

- `Block::device_class` reports the device's `blkio::BlkDeviceClass`
  (`Rotational`, `SolidState`, `Removable`, `Virtual`). Reporting it is a pure
  observation: it touches no hardware and cannot fail. The trait default is
  `Virtual`, the *unclassified* envelope — bounded rather than maximally
  patient, so a device whose driver says nothing about itself still fails
  closed promptly when it wedges.
- The class travels to the consumer in the **geometry completion**
  (`BlkCompletion::class`), alongside the device's size and write policy, and
  the consumer adopts it for every subsequent request: its per-request
  deadline, reissue budget, and the driver's grace window all derive from that
  one shared `BlkDeviceClass::budget` policy for *this* device (`AGENTS.md`
  §2.2, §24.1). Until the device answers, the geometry probe itself runs on the
  bounded unclassified envelope, so an endpoint that never answers fails closed
  promptly rather than being granted a spinning disk's patience on nothing but
  hope.
- The serving driver is untrusted, and this field needs no trust: the class
  selects only how patient the consumer is with this one device, grants no
  authority, and is bounded by the widest class budget either way. A driver
  that overstates its patience only delays its own deadline; one that
  understates it only fails itself sooner. An unrecognised class word on the
  wire decodes to the bounded unclassified envelope rather than being trusted
  with a wider one — and never discards an otherwise well-formed completion.
- **A device that wraps another reports what it wraps.** A partition window, a
  block cache, the kernel's disk-sharing boundary, the retained-writes
  journal, and the remote block clients all forward their inner device's
  class, so the real hardware's envelope survives every layer above it rather
  than being flattened to the default. A device *composed* of several others (a
  RAID array) declares the **most patient** of its live members
  (`BlkDeviceClass::most_patient`): the array can only answer as fast as the
  member it is waiting on, so a mirror of an SSD and a spinning disk is served
  the spinning disk's spin-up budget.

## Health state machine and the recovery grace window

A device that stalls, resets, or has its bus glitch is far more often only
*briefly* unwell than terminally dead, so a serving driver rides such a blip
out for a bounded **grace window** before failing it closed rather than
punishing the first missed beat (`plans/FIX-IO.md` IO3, `AGENTS.md` §26.5).
The policy and mechanism live in one shared place both a serving driver and a
consumer read (`blkio::BlkHealth`), never a per-driver copy (`AGENTS.md`
§2.2):

- `BlkHealth::observe(raw, now_ns)` folds each device-level outcome into an
  explicit `BlkHealthState` (`Healthy` → `Degraded` → `Recovering` →
  `{ Healthy | Faulted }` → `Offline`/`Removed` → `Failed`) and returns the
  `BlkStatus` the consumer is told. It is pure and event-timed: the caller
  supplies the monotonic `clock_get` reading, so there is no timer to spin on
  (`AGENTS.md` §2.23) and the whole machine is proven host-side.
- **Inside the window** a transient stall/reset is answered with a reissuable
  `BlkStatus::Reset` and the device is held `Recovering`, so a blip that
  resolves in milliseconds is invisible to the workload. The reply is
  reissuable *within its own per-request deadline* rather than parked, so one
  device's blip never stalls the serve loop's other units (head-of-line
  freedom, §26.1).
- **When the window elapses** without the device coming back the device goes
  `Faulted` and only *then* fails closed (`BlkStatus::Offline`). A device that
  has faulted stays quarantined until it *demonstrably* answers again, so a
  flapping disk cannot masquerade as healthy — yet a genuine return always
  recovers it to `Healthy` with no reboot (`AGENTS.md` §18.4).
- **A quiet device still expires its window.** `observe` advances the window
  when a request outcome arrives, but a device that stalls and then goes silent
  would otherwise sit `Recovering` forever. `BlkHealth::grace_deadline_ns`
  returns the absolute monotonic time the window closes (a driver arms a
  **one-shot** timer for it, never a busy-poll — `AGENTS.md` §2.23), and
  `BlkHealth::poll(now_ns)` is the pure, event-timed transition that fails a
  still-`Recovering` device closed to `Faulted` when that deadline passes with
  no further request. `observe` and `poll` share one `grace_elapsed` check so
  the request-driven and time-driven paths cannot diverge (`AGENTS.md` §2.2).
  A serving driver that parks between requests arms this from
  `tairix_abi::blkio::recovery_wait_timeout` — the soonest armed grace deadline
  across every unit it serves, relative to now — as its wait's one-shot
  timeout, so it wakes exactly when the nearest window is due and drives
  `poll`. The arithmetic is that one shared helper, never copied per driver
  (`AGENTS.md` §2.2).
- Only a *device-level* outcome drives health. A request-level rejection (a
  write to a read-only unit, an out-of-range LBA, a malformed frame) is
  classified `BlkStatus::for_driver_health(err) == None` and framed verbatim,
  so a hostile or malformed request can never drive a healthy device toward
  `Faulted`.
- The grace duration is **per-device-class policy** (`IoBudget::grace_ns` from
  `BlkDeviceClass::budget`), sized wider than the per-request deadline so a
  single reset/spin-up cannot exhaust it — a rotational disk's spin-up budget
  is not an SSD's. It is scaling policy, never one global `const` (`AGENTS.md`
  §24.1) and never a security/validation bound (§24.4).

The request engine is **one shared definition** every block driver reuses:
`tairix_abi::blkio::serve_request_recovering` decodes and validates a request,
drives the device through the `Block` trait, folds the outcome into a
`BlkHealth`, and frames the completion — so the validation, the fail-closed
refusals, the success paths, and the recovery grace window cannot diverge
between drivers (`AGENTS.md` §2.2, §27). It is pure and alloc-free, proven
host-side over in-memory `Block` doubles in `lib/abi`. `usb_msd` is the first
consumer: its wait-set serve loop hands each per-LUN request to the engine with
that LUN's `BlkHealth` (the `Removable` class) driven by the monotonic clock,
and arms its wait's timeout from `recovery_wait_timeout` so a LUN that stalls
and then goes quiet still has its grace window expired (`BlkHealth::poll`) and
fails closed on time — logged once, keeping the LUN's node and endpoint so its
consumer still receives typed fail-closed answers and a later genuine return
recovers it with no reboot (a health fault is not a surprise-removal, so the
node is not retracted). Only the usb_msd-specific block-service endpoint-id
derivation (`serve::blk_block_for`) lives in the driver crate. `virtio_blk` and
`emmc2` are currently consumed in-kernel (root-unlock) and expose only their
`Block` implementation; when either is brought up as a user-space serving
process it reuses the same engine and the same idle-timer helper rather than
copying them. Even in that in-kernel form each driver's `Block` already maps a
raw device status to the *honest* per-request health class rather than a blanket
`DeviceFault`: `virtio_blk` decodes a `virtio_blk_req` status byte through one
shared `status_to_result` (`VIRTIO_BLK_S_IOERR` → a per-request
`DriverError::MediumError` the consumer recovers around and repairs, not a
whole-device fault; `VIRTIO_BLK_S_UNSUPP` → `DriverError::Unsupported`; any
undefined status → a fail-closed `DriverError::DeviceFault`, never the benign
`Unsupported`), so the health axis is correct at the source and no consumer
drops a whole device over a single bad sector. Device-level health
observability through `lib/log`/`sysinfo` beyond the per-volume mount overlay is
the staged remainder (`plans/FIX-IO.md` IO3–IO6).

## The recovery-escalation ladder — what the *driver* does behind a blip

The grace window decides *what a consumer is told* while a device rides out a
blip; the **recovery ladder** decides *what the driver does to the hardware*
between the reissued attempts, so a stalling device is actively nudged back
rather than merely waited on. It is a second shared primitive,
`tairix_abi::blkio::RecoveryLadder`, owned per served logical unit alongside
that unit's `BlkHealth`:

- `RecoveryLadder::next_action(state)` is the single entry point mapping the
  unit's current `BlkHealthState` to the next `RecoveryAction`. An operational
  device (`Healthy`/`Degraded`) yields `None` and re-arms the ladder; a
  `Recovering` device **escalates** — the first attempt is a gentle `Retry`
  (a one-off comms glitch often clears itself, and a reset would only add
  latency), and each subsequent attempt is a data-path `Reset`; once the
  class's `IoBudget::max_retries` is spent it is `GiveUp` and the grace window
  is left to fail the device closed on time. A device already failed closed
  (`Faulted`/`Offline`/`Removed`/`Failed`) is `GiveUp`, so the driver stops
  escalating; a later genuine answer returns it to `Healthy` and re-arms the
  ladder.
- The ladder's cap is the **same** per-class `IoBudget::max_retries` the
  consumer's `IoBudget::should_reissue` reads, so the driver's escalation and
  the consumer's reissue budget derive from one policy and cannot drift apart
  (`AGENTS.md` §2.2). A device that keeps stalling therefore climbs a *finite*
  ladder and is never reset forever (`AGENTS.md`'s ban on
  retry-until-it-works).
- The ladder holds no clock or timer and never spins or parks: it advances one
  rung per reissued attempt, and reissued attempts are already spaced by the
  consumer's own reissue cadence and per-request deadline. That is *stronger*
  than a driver-side backoff timer for head-of-line freedom (`AGENTS.md` §26.1),
  since the serve loop never sleeps on one recovering device while its siblings
  wait, and it keeps the whole ladder provable host-side.

`usb_msd` is the first consumer: after replying to each request its serve loop
consults the LUN's ladder from the just-folded `BlkHealth` state and, on a
`Reset`, clears the unit's bulk pipes (`ScsiDevice::scrub_window` — this
driver's one data-path reset mechanism) and logs an `MSD_RECOVERY_RESET` audit
event. The reset is only ever issued for a unit already being answered
reissuably, so it cannot stall an unrelated LUN. Which concrete mechanism a
`Reset` maps to is per-driver (a virtio/NVMe driver re-inits its queue); an
action a driver's hardware cannot express is a no-op that still advances the
ladder, so the escalation is honest on every transport.

## Consumer-side bounded reissue

The grace window is the *serving* half of the reply-reissuable model; the
*consumer* half is a bounded reissue. When a serving driver rides a blip out it
answers within the request's own deadline with a reissuable status
(`BlkStatus::TransientError` / `Reset` / `Timeout`) rather than a hard fault, so
a consumer that simply surfaced the first such reply as an I/O error would
punish a device that was merely recovering. Instead every consumer of a served
block device — the kernel-side `BlkClient` (`kernel/core`) and the volume
manager's probe (`RemoteBlock`) — reissues a reissuable completion a bounded
number of times before failing closed:

- The retry count is the shared per-class policy `IoBudget::max_retries`, read
  through the one definition `IoBudget::should_reissue(status, attempts)` both
  consumers call, so they can never drift apart in when they retry versus fail
  closed (`AGENTS.md` §2.2). A device that keeps answering reissuably still
  fails closed deterministically at the budget rather than retrying forever
  (`AGENTS.md`'s ban on retry-until-it-works).
- Each reissue is a fresh post → park-on-reply exchange — it is event-driven,
  never a busy spin (`AGENTS.md` §2.23). The serving driver owns the recovery
  grace window and its timers; the consumer only honours the reissuable reply.
- A hard per-request deadline timeout (synthesised kernel-side when the driver
  never answers) and a torn-down endpoint fail closed with **no** reissue: a
  device that consumed its whole deadline without answering is treated as
  wedged, not retried. A non-retryable verdict — a `MediumError` bad sector, a
  gone `Offline`/`Removed` device — is surfaced on the first attempt.

## Fault domains — one hub/controller blip is one recovery episode

A bus, hub, USB controller, SAS/JBOD expander, or PCIe root complex owns a
group of block devices beneath it. When such an *owner* resets or blips, the
symptom on every disk below it is the same stall — so treating it as N
independent disk failures is wrong: it is **one** fault-domain event
(`plans/FIX-IO.md` IO4). `blkio::FaultDomain` is the interior-node counterpart
of the per-device `BlkHealth`, and both drive their recovery window through the
one shared `GraceWindow` timer, so an interior node and a leaf device time
their grace window identically and the arithmetic cannot diverge (`AGENTS.md`
§2.2).

- Which nodes are children is read from the discovered hardware tree
  (`lib/abi::hwtree`), never hard-coded — a USB hub, a SAS expander, and a PCIe
  root complex are all just interior nodes (`AGENTS.md` §18.1, §2.20). A
  `FaultDomain` stores only the owner's opaque node id, so the type stays
  platform-neutral.
- `FaultDomain::quiesce(now_ns)` opens **one** shared grace window over the
  whole subtree: every child's in-flight request is answered reissuably
  (`FaultDomain::child_status` returns `BlkStatus::Reset`), so a hub reset that
  resolves in milliseconds is invisible to the workload.
- `FaultDomain::resume()` records a *demonstrated* owner return: the whole
  subtree recovers to `Healthy` at once and children resume on their own
  per-device health. This is the only transition that clears a failed subtree,
  so a returning hub always recovers without a reboot (`AGENTS.md` §18.4).
- `FaultDomain::poll(now_ns)` fails a `Recovering` subtree closed to `Offline`
  when the window elapses, driven by the one-shot timer
  `FaultDomain::grace_deadline_ns` names rather than a busy-poll (`AGENTS.md`
  §2.23). A subtree that has failed closed is sticky until a demonstrated
  return, so a flapping hub cannot masquerade as healthy.
- The grace duration is **policy** the caller derives from the owner's
  discovered class (e.g. the widest child `IoBudget::grace_ns`), never one
  global `const` (`AGENTS.md` §24.1).

The `FaultDomain` machine is pure and event-timed (the caller supplies the
monotonic reading and drives the children's own `BlkHealth`), so the whole
coherent quiesce/resume is proven host-side in `lib/abi`.

Which interior node a device blips *with* is resolved by the shared
`hwtree::fault_domain_owner(nodes, node_id)` helper: it walks the discovered
hardware tree upward and returns the nearest strict ancestor that owns a group
of devices — a bus/hub/controller/expander/PCIe-root-complex
(`HwDeviceClass::Bus`), or the synthetic `Root` as the domain of last resort for
a device attached directly to it. It skips non-owning ancestors and fails
closed on an absent node, a rootless node, or a broken/cyclic chain (the walk is
bounded by the node count, never an unbounded spin, `AGENTS.md` §2.9). It reads
the tree and hard-codes no board (`AGENTS.md` §18.1, §2.20), so it is the one
definition every serving/bus driver uses to build a child's `FaultDomain`.

A device usually blips with more than one interior node — a disk on a hub on a
controller shares a fault domain with the hub *and* the controller *and* the
root. The **full ordered chain** of those nested owners, nearest first, is the
shared lazy iterator `hwtree::fault_domain_chain(nodes, node_id)`, built by
re-applying `fault_domain_owner` to each owner in turn — so a serving driver
builds one `FaultDomain` per interior node in the chain without re-deriving the
walk itself (`AGENTS.md` §2.2). It is allocation-free (it holds only a borrow of
the tree, so no fixed-depth ceiling, `AGENTS.md` §24.1), inherits
`fault_domain_owner`'s fail-closed behaviour at every level, and is cycle-safe:
bounded to at most one step per node, so even a malformed tree terminates rather
than spins. The chain is exactly the `domains` argument the two composition
helpers below consume.

Two pure composition helpers let a serve loop use those fault domains exactly
as it already uses the per-device machinery, without re-deriving the rules
(`AGENTS.md` §2.2):

- `blkio::fault_domain_wait_timeout(domains, now_ns)` is the interior-node
  counterpart of the per-device `recovery_wait_timeout`: the soonest armed
  subtree grace window, relative to now, so a serve loop parks on the nearest
  event and never leaves a quiesced-but-quiet domain `Recovering` forever. Both
  delegate to one shared `nearest_relative_deadline` core, so a loop that owns
  *both* per-device and fault-domain windows takes the min of the two and
  cannot compute them by different rules (`Some(0)` = poll now, `None` = park
  with no timeout, matching the `waitset_wait` convention).
- `blkio::effective_child_status(device_status, domains, now_ns)` folds a
  child's own outcome with what each ancestor imposes into the one status its
  completion carries, using `BlkStatus::combine`'s total order
  (`BlkStatus::severity`). A hub mid-reset turns a child's `Ok` into a
  reissuable `Reset` (its aborted data is not consumed); an ancestor whose
  window has elapsed fails the child closed to `Offline`; and a device's own
  definitive `MediumError` still wins over a concurrent reset — a bad sector is
  real and must not be retried into. The fold is associative and commutative,
  so a deeper failing domain can never be masked by a shallower healthy one,
  whatever order the chain is walked in.

`BlkStatus::severity`/`combine` are the single, explicit definition of "which
health signal wins" when more than one applies to one request, kept independent
of the wire value `BlkStatus::as_u32` so the transport encoding and the recovery
precedence can never silently couple. All of these are pure and proven
host-side in `lib/abi`.

A `FaultDomain` owner need not be a *bus* node in the tree: a leaf driver's own
shared transport that fans out to several logical units is equally a
fault-domain owner of those units. `usb_msd` is the first live consumer — every
LUN behind one USB mass-storage device shares one Bulk-Only pipe pair, so the
data-path reset it escalates is a transport-wide event. Its serve loop owns one
`FaultDomain` for that shared transport (owner = the device's own discovered URB
transport grant), `quiesce`s it around the reset, drives each LUN through the
per-request engine and folds the domain's verdict with `effective_child_status`,
`resume`s the whole device when any unit completes a real transfer, arms its
wait from the min of `recovery_wait_timeout` and `fault_domain_wait_timeout`, and
audits the device-wide edges through `BlkHealthTransition::for_fault_domain`
(`drivers/storage/usb_msd/src/recover.rs`). So one shared-transport blip is one
recovery episode across the device, not N spurious LUN failures.

The first live *interior hardware-tree node* consumer is the **xHCI host
controller** (`drivers/bus/usb/xhci`). The controller is the interior node every
USB device below it hangs from, so a controller-wide fault — a latched Host
System Error / HCHalted, or the `HCRST` reset the driver performs to recover — is
one recovery episode over the whole subtree. Its pure, host-tested
`domain::ControllerHealth` coordinator wraps one `FaultDomain` (owner = the
controller's own discovered URB endpoint-block base; grace =
`CONTROLLER_GRACE_NS`, matching the removable-storage window it sits above) and
the freestanding serve loop drives it around the controller reset: it
`begin_recovery`s on the first fault, arms its wait from `wait_timeout` and
retries on the grace one-shot (the fix for a faulted controller raising no
further interrupt, xHCI §4.24.1, which previously parked the loop forever),
`note_reset`s each attempt (recovering on a demonstrated return, failing closed
once the window elapses), and audits the device-wide edges through
`BlkHealthTransition::for_fault_domain` (`HCD_DOMAIN_RECOVERING` /
`HCD_DOMAIN_RECOVERED` / `HCD_DOMAIN_OFFLINE`). A controller failed closed stays
sticky-but-recoverable — a later successful reset clears it — and is not retried
against forever.

An interior node's fault-domain state reaches the leaf block consumers beneath
it through the discovered hardware tree itself: `HwNode::fault_health` carries a
`FaultDomainState` on the wire, and an interior-node driver publishes its *own*
node's health with the `hw_node_health` syscall (`CAP_HW_EMIT`, audited; the
kernel resolves the caller's own matched node, so a driver can never forge
another's health). Recording it bumps the hardware-tree generation, so the
reactive `hw_tree_wait` observers re-read — the same channel the hotplug
emit/remove path uses, but a *distinct* signal: the node stays present, only its
health changes, so a merely-recovering subtree is never torn down. The xHCI
controller is the live emitter (each `ControllerHealth` edge → a
`Recovering`/`Healthy`/`Offline` publish); the device manager is the live
consumer — a bound child whose fault-domain owner (recorded per binding via
`fault_domain_owner`) is currently `Recovering` is **held**, not unloaded, when
it transiently vanishes, so one controller reset is one recovery episode across
the subtree rather than N spurious teardown/reload cycles. The affected volumes
already surface as `Recovering` through the kernel `BlkClient`'s existing
`MountAvailability` fold as their leaf transports blip.

The remaining live wiring is the deeper nested-owner chains a hub or SAS
expander adds (`fault_domain_chain` + `effective_child_status`) and the QEMU
vertical that exercises the whole subtree recovery (`plans/FIX-IO.md` IO4–IO6).

## `BufferClass` and zero-on-free

`*_with_class` accept a `BufferClass` (`NonSensitive` /
`Sensitive`). Per `AGENTS.md` §4 a driver that bounces payload
through an internal staging area **must** scrub that staging before
the method returns when `class == Sensitive`. The default
implementations of the `_with_class` methods delegate to the plain
methods and are only safe for drivers that DMA straight into the
caller-owned buffer; drivers that bounce-buffer (such as
`virtio_blk` over the Stage 4 host-side allocator) override them.

The trait makes no guarantee about scrubbing the caller-owned `buf`;
that remains the caller's responsibility once it has consumed the
payload.

## Sharing one device across windows

The boot path brings up exactly **one** bootstrap-floor block device, yet two
independent consumers must read it during bring-up — the read-only signed
`/System` driver-store mount and the encrypted-root unlock window — and, under
Design D, the `/System` store must stay reachable for on-demand and reactive
(hotplug) driver loads (`AGENTS.md` §18.3 / §18.4). One disk must therefore
back two concurrent partition windows.

The kernel block-sharing layer (`tairix_kernel::shared_block`) is that
primitive. A `SharedBlock<B>` owns the brought-up device behind a `lib/sync`
`SpinLock` and hands out `SharedBlockHandle`s, each of which is itself a
`Block`. Every byte-moving operation takes the lock for the duration of one
device call, so concurrent windows on different CPUs are serialised
(`AGENTS.md` §4 — SMP from day one). The device's `BlockGeometry` is immutable
for the life of a disk, so it is queried once at construction and cached:
`geometry()` is then lock-free (`AGENTS.md` §2.16). A geometry fault at
construction refuses to wrap the device, so no handle is ever handed out for
an unusable device (fail closed, §2.9).

A plain `SpinLock` (not the IRQ-safe variant) is correct because block I/O is
driven from task / kthread context — the device IRQ only *wakes* the waiting
kthread, it never issues a transfer from inside the handler — so the lock is
never taken from an interrupt. The layer is generic over any `Block` and names
no device or architecture, so every port shares the one definition (§2.2 /
§2.20). The aarch64 root-unlock tail (`finish_unlock`) wraps its brought-up
virtio-blk or EMMC2 device in a `SharedBlock` and drives both the `/System`
autoload and the interactive unlock through concurrent handles rather than
borrowing then moving the one device.

## The persistent driver-store service

Design D needs the `/System` driver store reachable for the life of the system
(on-demand and reactive driver loads, `AGENTS.md` §18.3 / §18.4), not only
during boot. `DriverStoreService<B>` (`tairix_kernel::shared_block`) owns the
boot disk's `SharedBlock` and hands out a fresh read-only window
(`SharedBlockHandle`) for each `/System` read.

It keeps the mount alive **without promoting the device backing to `'static`**.
The aarch64 root-unlock kthread is a *never-returning* kernel service
(`AGENTS.md` §17.1 — "a continuous service never returns"): because
`finish_unlock` receives the brought-up device by value while its backing (the
DMA pool, MMIO map, IRQ waiter, and virtio host, or the EMMC2 register-window
map) stays on the still-suspended `virtio_blk_unlock` / `emmc2_unlock` frame,
making `finish_unlock` never return keeps that whole bring-up call chain
suspended on the kthread's coroutine stack. The borrowed backing therefore
stays live for free, and the proven IRQ-wait / cooperative-yield device-driving
model is unchanged (`AGENTS.md` §2.17 — no security or correctness regression
on a metal-confirmed path).

After running the boot autoload and the encrypted-root unlock through two
concurrent windows, logging the outcome, and releasing the console-0 gate to
`login`, the service calls `DriverStoreService::hold`, which **parks** the
kthread for life owning the `SharedBlock` — a real park, never a busy-yield
loop (`AGENTS.md` §2.1), so it consumes no CPU while idle. A later reader (the
D2b `driver_store_load` path) wakes this kthread to serve a `/System` read
through a window and then re-parks, reusing the one proven I/O path rather than
driving the device from an arbitrary caller's context.

## Shipped drivers

| Driver                                   | Crate                                | Supported buses     | Status                                   |
|------------------------------------------|--------------------------------------|---------------------|------------------------------------------|
| [virtio-blk](./virtio.md)                | `tairix-drv-storage-virtio-blk`      | virtio (PCI / MMIO) | host-side tests + mock transport only    |
| Raspberry Pi 4 EMMC2                      | `tairix-drv-storage-emmc2`           | Pi 4 SDHCI (MMIO)   | ADMA2 DMA + PIO read/write host-tested; interrupt-driven; wired into root-unlock over DMA; DMA metal acceptance pending (Pi 4) |
| USB mass storage (BOT / CBI / UAS)        | `tairix-drv-storage-usb-msd`         | any USB host via the URB transport | shared SCSI layer + three wire transports (incl. UFI floppies) host-tested over scripted doubles; metal acceptance pending (Pi 4) |

QEMU integration on real PCI / MMIO virtio devices depends on the
prerequisites enumerated in `.junie/next-session-prompt.md` (kernel
DMA, IRQ routing, bus-handle hand-off).

### Discovery and the bootstrap floor

Every shipped block driver publishes a canonical `BIND_KEYS` table
(`AGENTS.md` §18.3) so a discovered hardware-tree node binds them by
match, never by a kernel guess (§18.5):

| Driver       | `BIND_KEYS` match key                         | Discovered node source                          |
|--------------|-----------------------------------------------|-------------------------------------------------|
| virtio-blk   | virtio device id `2` (`HwMatchKey::virtio(2)`)| a probed virtio node (PCI or MMIO transport)    |
| EMMC2        | `compatible = "brcm,bcm2711-emmc2"`           | the aarch64 `FdtDiscovery` Storage node         |
| USB MSD      | USB class `08:06:50` (`HwMatchKey::usb(0, 0, 0x08_06_50)`) | the mass-storage interface node the xHCI HCD emits |

The virtio-blk and EMMC2 drivers are part of the **bootstrap floor** (`AGENTS.md`
§18.6): the storage path must be up before the signed driver store under
`/System/Drivers/` is reachable, so the volume that holds the store can be
read. They are therefore compiled in and registered in the kernel binary's
`driver_catalog::IN_KERNEL_DRIVERS` floor registry (virtio-blk for the QEMU
`virt` / x86_64 root, EMMC2 for the Raspberry Pi 4 SD card), each paired
with the driver crate's own `BIND_KEYS` and a build-signed manifest. The
floor binds by discovery-match through the same shared `lib/devmatch`
policy the user-space `devmgr` uses — the in-kernel match and the
user-space match can never diverge (§2.2) — and is signature-verified and
capability-gated alike (§18.6). The floor only ever shrinks toward the
store, never grows.

### Raspberry Pi 4 EMMC2 (SDHCI)

`tairix-drv-storage-emmc2` brings up the Pi 4 (BCM2711) EMMC2
controller — an Arasan / SDHCI-5.1 SD host — and exposes the card
through `Block`. The fast transfer path is **32-bit ADMA2 DMA**: the
controller masters a whole 64 KiB chunk (`DMA_STAGE_BLOCKS` = 128 blocks)
over the DAT lines through a one-entry ADMA2 descriptor the engine stages
in a device-shared bounce region (`SdhciHost::dma_region`, `adma`), so a
multi-block transfer completes on a single transfer-complete interrupt
instead of a per-block buffer handshake and the CPU never moves data
word-by-word through the slow uncached buffer data port; larger requests
loop over the chunk. The engine supports both coherent/Normal-Non-Cacheable
regions and cacheable slabs carrying a `DmaSlab` coherency callback. It
synchronizes the data range and descriptor before `dma_wmb` plus the
doorbell, then after a read completion performs `dma_rmb` and synchronizes
the device-written data before consuming it. The Pi 4 bootstrap host's
callback runs aarch64 `dc civac` cache maintenance because EMMC2 does not
snoop the CPU caches; coherent hosts use the no-op path. When the host
grants no DMA region the engine falls back to
**programmed I/O** through the buffer data port (`CMD17`/`CMD18` reads,
`CMD24`/`CMD25` writes), which needs no DMA capability — DMA where
possible, correct everywhere (`plans/PI.md` P8). The command/transfer-mode
encoding is shared between both paths (`read_command`/`write_command`,
§2.2).

The state machine (`Emmc2`) is written against the `SdhciHost` register
seam, so it is proven host-side against a register-level mock controller
and runs on metal over a capability-gated `RegisterWindow` mapped by
`wiring::open_discovered` from the device-tree-discovered
`brcm,bcm2711-emmc2` node (`AGENTS.md` §2.2 / §18.3). There is no
Pi-board QEMU vertical (QEMU does not model EMMC2, `plans/PI.md` §0.4);
the emulation artefact is the host test, including exact single- and
multi-chunk cache-synchronization ranges, and metal acceptance is the
documented bring-up checklist. `Emmc2::open` runs the standard SD
identification (`CMD0`/`CMD8`/`ACMD41`/`CMD2`/`CMD3`/`CMD9`/`CMD7`/`CMD16`)
and derives geometry from the card CSD; only high-capacity,
block-addressed (SDHC/SDXC, CSD v2) cards are supported and anything
else is rejected fail-closed.

Identification runs at the SD identification clock (≤400 kHz) on the 1-bit
bus the controller resets to. Once the card is selected, two pure speed
steps run before any block transfer: `ACMD6` switches the card to the
4-bit bus (the controller's `CONTROL0` data-width bit set to match, 4×),
and the SD clock is raised to the data divisor (`DATA_CLOCK_DIVISOR`,
derived as `IDENT_CLOCK_DIVISOR / 32` so the data clock is 32× the
identification clock — ≤12.8 MHz, within SD Default Speed's 25 MHz, no
high-speed switch needed). This turns the ~50 KB/s identification-clock
1-bit path into the ~6 MB/s Default-Speed 4-bit path the driver-store scan
and every bundle read inherit (`AGENTS.md` §2.16); the divisor is derived
from the identification divisor, not a base-clock constant, so it carries
no board assumption (`AGENTS.md` §2.20).

Command- and transfer-completion waits **park on the controller's
interrupt** through a `CompletionWait` seam (`SdhciHost::await_irq`) rather
than busy-spinning a status register, so a slow SD operation never
monopolises the CPU and starves interrupt-driven work (`AGENTS.md` §17.1 /
§2.16) — the defect that froze the boot UART log while `/System` was being
read during driver autoload. `reset_and_clock` enables the controller's
completion-signal sources (`IRPT_EN`) so it raises its CPU interrupt line
on each completion and on every error bit; the kernel supplies the
`CompletionWait` that binds, routes, arms, and parks on that GIC line
(`emmc2_unlock`, below). The remaining identification-only register
handshakes that have no completion source (reset, clock-stable) still spin,
and every wait is bounded by a poll budget that fails closed with
`DriverError::DeviceFault` rather than waiting forever (`AGENTS.md` §2.1).

Bring-up resets the host controller and then **powers the card rail**
(SD Bus Power on, 3.3 V) through the power-control byte of `CONTROL0`
*before* clocking the bus. The full host-controller reset clears SD Bus
Power, and the standard SDHCI register block gates all command/data
activity on it, so without this write the very first command (`CMD0`)
never completes (the bus is dark) — the failure a real Pi 4 reported at
`stage=CMD0 GO_IDLE_STATE`. Linux's Pi 4 EMMC2 brings the same power
register up to `0x0F`.

The CSD geometry decode reads the R2 response **exactly as the controller
lays it out**: for a 136-bit response the SDHCI block strips the 8-bit CRC
tail and right-aligns the remaining 120 bits across `RESP0..3`, so
`CSD_STRUCTURE` (CSD[127:126]) lands at `RESP3` bits [23:22] — not the top
of the word, whose high byte is zero padding — and `C_SIZE` (CSD v2) at
`RESP1` bits [29:8]. Reading the structure field at the wrong position made
a real Pi 4's valid SDHC card decode as an unsupported structure and fail
at `stage=CMD9 SEND_CSD`; the decoder now reads the correct bits, and the
host mock models the same right-aligned layout so the regression cannot
recur.

Because there is no Pi-board QEMU vertical, the only signal that localises
a bring-up failure on a real Pi 4 is the UART log. `Emmc2::open` therefore
fails with a `BringUpFault` that pairs the underlying `DriverError` with a
`BringUpStage` naming the exact SD-identification step that stalled (map
register window, reset + SD clock, `CMD0`, `CMD8`, `ACMD41`, `CMD2`, `CMD3`,
`CMD9`, `CMD7`, `CMD16`, `ACMD6` set-bus-width, raise SD clock). A consumer
that only needs the §8 `DriverError`
drops the stage with `?` / `DriverError::from`; the in-kernel root-unlock
path instead logs `BringUpStage::as_str` as a structured `stage=` field
(`AGENTS.md` §2.16 — measure, do not guess).

The driver is **wired into the root-unlock path** (`plans/PI.md` B4): when
the root-storage bind gate binds the `brcm,bcm2711-emmc2` node, the aarch64
root-unlock kthread (`crate::aarch64::root_unlock::emmc2_unlock`) maps the
node's sole SDHCI register window under `CAP_MMIO_MAP` through a minimal
in-kernel `DriverHost` that also carves the ADMA2 staging slab from a
`CAP_MEM_DMA`-gated per-driver DMA pool (`Emmc2DmaHost`), admits the driver
through the signed §8 load gate, **discovers the controller's GIC SPI from the firmware device
tree (`emmc2_spi`) and binds, routes, and arms it on the published IRQ
table** — supplying the driver a `CompletionWait` (`Emmc2Completion`) that
blocks on that line through the same task-parking waiter the virtio
bring-up uses (`tairix_kernel_core::IrqParkWaiter`, §2.2): a syscall-context
wait parks its task off the run queue (woken by the ISR's `irq_wake`), a
boot-kthread wait takes the bounded race-free `wfi` fallback, and a
controller silent past the 2 s budget fails the transfer closed as
`DriverError::DeviceFault` — opens the card, and feeds
the resulting `Block` to the same mount + `/System` autoload +
interactive-unlock tail as virtio-blk (`finish_unlock`, §2.2). With no EMMC2
interrupt in the device tree the bring-up fails closed rather than parking
on a line that can never fire (`AGENTS.md` §2.9 / §18.4). On a bring-up
failure it logs the failing
`BringUpStage` as the `stage=` field of the `EventId(4139)` unlock-service
error line together with the underlying `DriverError` as an `error=` field,
so the metal UART log names both the SD command the card stalled at and how
it failed — distinguishing a controller/command fault (`error=device
fault`) from a decode rejection (`error=unsupported`) at the same step.
Since `raspi4b` cannot model EMMC2, that live bring-up is metal-gated; the
host test and the §0.9 metal checklist are the acceptance artefacts.

### USB mass storage (BOT / CBI / UAS) — `drivers/storage/usb_msd`

`tairix-drv-storage-usb-msd` is the first **discovered-tier, user-space**
block driver (`plans/DEVICES.md` D2/D5): a pure USB *class* driver `devmgr`
autoloads against the mass-storage interface node the xHCI host-controller
driver emits. It owns no register window, no DMA, and no IRQ — every
transfer rides the bus-agnostic URB transport (`lib/usb`), so the same
binary serves a disk behind any host controller that speaks it.

The driver reads the device's own configuration descriptor to derive the
interface number, wire transport, command set, and endpoints (never
assumed), then drives one transport-neutral SCSI command layer
(`src/scsi.rs` — the transparent set, or UFI's 12-byte padded CDBs and
`MODE SENSE(10)` for floppies) over the transport the device speaks:

- **Bulk-Only Transport 1.0** (`08:06:50`, `08:04:50`): each command
  wrapped in a CBW on bulk-OUT, the data phase over the bulk pair in
  bounded chunks, and the CSW validated field by field (signature, tag
  match, residue bound, status) — the device is hostile input. A stalled
  data phase falls through to the CSW; a stalled CSW read is retried once;
  a tag mismatch, corrupt CSW, or phase error runs the spec's Bulk-Only
  Mass Storage Reset and fails the command closed.
- **Control/Bulk/Interrupt 1.1** (`08:04:00`, the classic USB floppy):
  the 12-byte command block over the ADSC control-OUT data stage (a
  control STALL is the device's "command not accepted" answer, recovered
  in place by the URB layer), the data phase over the bulk pair, and the
  two-byte command-completion interrupt (UFI ASC/ASCQ, or the typed
  status spelling for non-UFI sets); a malformed or out-of-step
  completion runs the spec's Command Block Reset. A UFI failure's
  ASC/ASCQ is read **in-band** from that completion interrupt (like UAS
  autosense), so the command layer never issues a separate `REQUEST
  SENSE` — a real UFI floppy does not answer one reliably, and depending
  on it aborted floppy bring-up on hardware.
- **USB Attached SCSI** (`08:06:62`): the four Pipe-Usage-named bulk
  pipes with tag-checked Command / Read-Ready / Write-Ready / Sense IU
  sequencing (USB 2.0 non-stream operation) and in-band autosense; every
  IU is validated fail-closed — a foreign tag, wrong-direction ready IU,
  or lying sense length refuses the exchange. One command is in flight at
  a time (the block service is synchronous); queueing, task-management
  IUs, and SuperSpeed streams are the staged remainder (`plans/DEVICES.md`
  §3).

Per logical unit (`GET MAX LUN` for BOT, `REPORT LUNS` for UAS, exactly
one for CBI; up to 16) the bring-up runs `INQUIRY` (non-disk types are
skipped), a bounded ready drain (the start-of-day not-ready / UNIT
ATTENTION states drained, the sense consumed per failed attempt — in-band
for UAS and CBI/UFI, via `REQUEST SENSE` for BOT),
`READ CAPACITY(10)`/`(16)` with a fully validated geometry (power-of-two
block size 512–4096; the 16-byte form covers units past the 32-bit LBA
horizon), and the command set's write-protect bit — enforced driver-side
(`DriverError::PermissionDenied` before any byte reaches the device), not
merely reported.

Each ready LUN is published as a **storage-class hardware-tree node**
(compatible `tairix,usb-msd-lun`) carrying two grants: a block-service
call endpoint and a 32 KiB shared data window. Consumers drive the unit
with the fixed-frame `tairix_abi::blkio` protocol (`BlkRequest`:
geometry / read / write / flush; completions carry the geometry and the
read-only flag) — the same request-reply IPC shape as the URB transport,
served by the driver's wait-set loop (never a busy-poll). Each LUN carries a
per-unit `blkio::BlkHealth` (the `Removable` device class), so a transient
device stall or bus reset is ridden out through its recovery grace window —
answered reissuably while the unit is `Recovering` — and only a unit that
stays unwell past the window is failed closed to its consumers, while the
other LUNs and every other mount keep running (`plans/FIX-IO.md` IO3). A
hot-unplug surfaces as the URB endpoint vanishing: the driver retracts its
LUN nodes and exits cleanly so a re-plug re-enumerates and reloads it. The
engine, descriptor reader, and block service are host-proven over scripted
doubles; the live path is Pi 4 metal acceptance (QEMU models no Pi USB).

### Volume manager (automount policy) — `drivers/storage/volmgr`

`tairix-drv-storage-volmgr` closes the hotplug loop (`plans/DEVICES.md`
D3c): it is the **policy driver** `devmgr` autoloads against each per-LUN
block-service node (compatible `tairix,usb-msd-lun`), one instance per
node, spawned with exactly that node's blkio endpoint + shared-window
grants — the same discovery/match/grant machinery every driver uses, so
no new kernel surface and no ambient authority (an instance can never
reach a sibling device's transport; the per-endpoint grant gates every
`ipc_call`).

The instance is a **read-only prober**: a fail-closed blkio `Block`
client (hostile geometry refused at connect, `write_blocks` refuses by
construction), the layout probe (whole-device filesystem signature first
— a superfloppy — else the GPT/MBR table via `lib/partition`, each
present partition's head probed by content through `lib/fsprobe`;
declared partition types are hints the probe ignores), and the
deterministic naming policy (the volume's own label sanitised through
the alias character rules, else `<fstype><n>`; a name collision appends
the volume-identity fingerprint, lengthened per retry, so re-inserting
the same volume re-derives the same name).

It runs in **two ordered phases**, because the probe and the mounts it
creates would otherwise be two concurrent users of the device's one staging
window (see "The shared data window has exactly one user at a time"). First
the whole device is probed and every recognised volume recorded; then the
blkio transport is *dropped*, which consumes the window borrow so no further
probe read can compile; only then is each recorded volume handed to the
kernel through the `CAP_FS_MOUNT`-gated, audited
`volume_attach` syscall — the kernel re-validates the grants, extent,
and name, opens the filesystem itself, mounts under `/Storage/<name>`,
and publishes the durable `id::` root. The instance then exits `0`
(run-to-completion; the kernel-held mount outlives it), logging every
outcome with stable event ids (4180–4184). Removal handling (surprise
removal, retained dirty state, force-unmount, verified re-insert) is the
staged D4 work.

The blkio client, probe plan, and naming policy are host-proven over
scripted devices and synthetic disk images; the live path is Pi 4 metal
acceptance, following the `usb_msd` precedent.
