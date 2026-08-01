# `tairix-drv-storage-raid-member`

TAIRiX **RAID member-agent driver**: the autoloaded per-disk driver that
delegates one array member's block transport to the array composer
(`plans/FIX-IO.md` `IO6c`; `docs/src/lib/raid.md`).

The composition arithmetic is not here. The six levels, the `RaidArray`
dispatch, the assembly bridge and the maintenance scheduler are the shared
`lib/raid` crate; the array composer that assembles discovered members into
served arrays is the sibling driver crate `drivers/storage/raid`. This crate
is only the member's own agent: the bind table its signed bundle publishes,
the pure decision logic, and the `Run` program the kernel spawns for a
matched node.

## Supported hardware

None directly. It binds no device and issues no MMIO, DMA, or interrupt: it
binds a *node*, and the storage beneath it is reached only through the public
block ABI by the process it hands the transport to. It is therefore
bus-neutral and vendor-neutral by construction.

## What it does

An array is several block devices driven as one, so one process must hold
client authority over every member at once. A driver is spawned for exactly one
matched hardware-tree node and receives exactly that node's resource grants, so
no process is born able to reach a whole array. The member agent closes that
gap without widening anyone's authority.

Matched to a `tairix,raid-member` node — the node the volume manager emits for
a device whose own first block probed as array metadata — one instance:

1. Resolves its two grants: the device's block-service call endpoint and its
   shared data window.
2. **Delegates** both to the array composer's reserved rendezvous
   (`call_grant`, `shm_grant`) and posts a `MemberOffer` naming them.
3. Holds the membership open. The composer answers only when the membership
   ends, so one outstanding call carries the whole lifecycle: the agent parks
   on the reply, and the composer's endpoint being torn down cancels the call
   and wakes it.
4. On a release or a vanished composer, offers again on a bounded escalating
   cadence. On a refusal it stops: that verdict came from reading the device
   itself, so the same unchanged device would only reach it again.

Nothing polls: the reply and the cancellation are events, and the only timed
wait is the paced re-offer when no composer is listening yet. The pacing is the
shared `tairix_raid::RetryCadence`, the same escalation an array uses to
re-probe a faulted member, so the two cannot drift.

The agent never reads or writes the device. Which array a device belongs to,
which slot it holds and which generation it last saw are read from the device
itself by the composer, through the shared on-disk metadata definition — never
taken from the agent, which is an ordinary user-space process.

## Required capabilities

`CAP_IPC_ENDPOINT` (delegate its one granted block endpoint and post the
offer), `CAP_SHM` (delegate its one granted data window), and `CAP_LOG_EMIT`
(diagnostics). No MMIO, DMA, IRQ, node-emission, or mount authority, and no
filesystem access.

Two properties bound what a compromised agent could do. It can delegate only a
resource it already holds a grant for, so it can never hand over another
device's transport; and the rendezvous id is reserved, so only a holder of
`CAP_IPC_BIND_PRIVILEGED` can be on the receiving end. Without that second
gate an unprivileged squatter that claimed the id first would be handed
read/write authority over every array member on the machine as each agent
delegated to it in turn.

This is also why the agent is its own bundle rather than sharing one with the
array composer: one signed bundle grants its whole manifest's capability set
to every instance loaded from it, and one instance of this driver runs per
member disk, so a shared bundle would hand every per-disk agent the
composer's `CAP_IPC_BIND_PRIVILEGED` and `CAP_HW_EMIT` authority it has no
need of.

## Runtime load and unload

Loadable and unloadable at runtime like any other bundle. The instance's
lifetime is its member's presence: when the device goes, its node goes, and
`devmgr` unloads the instance. While the device is there, the agent is what
lets a restarted composer reassemble the array without a reboot.

## Tests

The agent's lifecycle is proven host-side over its pure decision logic
(`src/agent/tests.rs`): the first offer is immediate, a delivered offer parks
with no deadline, an undelivered one is paced and escalates to a bounded
ceiling, a release and a vanished composer both lead to a re-offer, a refusal
stops the agent for good, and a successful offer clears a previous outage's
escalation. The escalation arithmetic underneath is proven once in `lib/raid`.

The live path — a real delegation to a real composer — arrives with the
composer's serving half, staged in `plans/FIX-IO.md` `IO6d` onward.

## Scope

The member agent only. The array composer — matched to the synthetic virtual
bus, connecting to each offered device, composing each array through the
shared engines and publishing it as its own block-service node — is the
sibling `drivers/storage/raid` crate, staged as `IO6d` onward in
`plans/FIX-IO.md` §2.6.

## Stability

`experimental` — the agent lifecycle is complete and host-tested; the live
path waits on the composer's serving half to land.
