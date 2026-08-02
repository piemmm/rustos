# `tairix-drv-storage-raid`

TAIRiX **RAID array-composer driver**: the autoloaded policy driver that turns
discovered array members into served arrays (`plans/FIX-IO.md` `IO6`;
`docs/src/lib/raid.md`).

The composition arithmetic is not here. The six levels, the `RaidArray`
dispatch, the assembly bridge and the maintenance scheduler are the shared
`lib/raid` crate, because the native filesystem's multi-device volumes drive
the same engines. This crate is the driver: the bind table its signed bundle
publishes, the pure decision and live-array logic, and the `Run` program the
kernel spawns for the matched node. The per-disk agent that delegates a
member's transport to it is the sibling crate `drivers/storage/raid_member`.

Stability tier: **experimental**.

## Supported hardware

None directly. It binds no device and issues no MMIO, DMA, or interrupt: it
binds the kernel's synthetic `tairix,virtual-bus` node, and every disk it
composes is reached only through the public block ABI over a transport some
other process already held and delegated. It is therefore bus-neutral and
vendor-neutral by construction, and works over any device whose driver serves
the block ABI.

## What it does

One instance runs per machine. The kernel's hardware-tree bootstrap publishes
exactly one synthetic virtual bus, so `devmgr` matches this driver to exactly
one node and the composer starts whether or not any array member exists yet.

1. It binds the reserved rendezvous endpoint and waits. Each per-disk member
   agent delegates its device's block endpoint and data window there and posts
   a `MemberOffer` naming them.
2. For each offer it maps the window, connects a read/write block client, and
   **reads that device's own superblock itself**. A member node says only
   "look here": which array a device belongs to, which slot it fills and how
   current it is are read off the disk, never taken from the offering agent.
3. The registered members feed `MemberRegistry`, whose `next_action` says what
   to do: assemble a ready array, place a member that turned up late into an
   array already serving, or wait until a deadline.
4. An array it assembles gets its own block-service endpoint and shared data
   window, and is published as a `tairix,raid-array` storage node carrying
   both. The volume manager binds that node exactly as it binds a disk's, so
   the array's filesystems mount through the unchanged path — and an array can
   itself become a member of another array, because its node is
   indistinguishable in kind from a disk's.
5. It then serves every live array's block requests through the same
   fault-aware engine a leaf device is served with, so an array is as
   fault-aware as a disk and there is no second serve path.
6. Between requests it gives each array one bounded turn of **self-maintenance**
   — re-admitting a member whose backoff has elapsed, advancing a rebuild,
   verifying the array, or writing down where it has got to — and records every
   change in what the array can promise.

The membership is the agent's own parked call: an accepted member's offer is
held open and answered only when the membership ends, so no separate liveness
protocol exists. A member the registry refuses is answered at once. An agent
re-offering a device is also taken as proof that disk is back, which brings its
re-probe forward instead of leaving it to wait out an escalated backoff — the
commonest reason a re-offer cannot be placed is that its slot still holds that
very device as a faulted member.

Nothing polls. One wait-set carries the rendezvous and every live array's
endpoint, and the single park's timeout is the soonest of the registry's
settle/backoff deadline, the arrays' maintenance deadlines, and their recovery
grace windows. A turn that actually moved a rebuild or a verification pass
forward comes straight back round instead of parking, so an idle array heals at
full speed while a busy one keeps yielding to its workload — but every such turn
does real I/O, so the loop is a worker, never a poll.

## Data-integrity rules

Every rule below exists because breaking it loses data silently rather than
loudly, so they are stated in one place.

- **An array that cannot answer for itself is never published.** A stripe
  missing a member, or a RAID5 missing two, has holes no redundancy can fill.
  `RaidLevel::can_serve` (`lib/raidmeta`) is the single definition of that
  question, and an array failing it is left unassembled rather than brought
  online short.
- **A complete array is composed at once; an incomplete one settles first.** A
  member that is merely spinning up, or riding out a bus blip inside its own
  driver's grace window, is not a missing member, and starting without it
  forces a needless rebuild. The wait is that hardware's own recovery grace
  window (`RetryCadence::for_class`, folded over the members' declared classes
  with `BlkDeviceClass::most_patient`), and it runs from the array's first
  member, so widening it for a slow disk can never become an indefinite
  postponement.
- **A start that cannot see a member fences it.** When an array comes online
  with a slot missing, its generation is bumped and every surviving current
  member's superblock is rewritten at the new generation before the array is
  composed. The disk that is missing — which may still hold a superblock
  claiming it is current — therefore keeps its lower generation and resolves as
  the stale rebuild target it is when it returns. A re-stamp that cannot be
  written fails the whole bring-up rather than serving an array whose metadata
  lies. A slot that is *present* but behind needs no fencing: its own superblock
  already records it, and moving the array again on its account would discard
  the recorded position of the very rebuild it is the target of, so a restart
  mid-rebuild would start that rebuild over every time.
- **A finished rebuild is recorded.** The moment a member's rebuild completes,
  its superblock is stamped current. An array whole in memory but still short a
  copy on disk would rebuild that copy from scratch on the next start — for
  hours, on a large array, for ever. A refused stamp is reported and the array
  keeps serving correctly: the unrecorded member stays *behind* on disk, which
  is the safe direction, and it simply rebuilds again later.
- **A member's own metadata is not array data.** Every member is composed
  through a `tairix_partition::PartitionBlock` view that begins past the
  reserved metadata blocks, so no array read or write can reach the superblock
  or the maintenance record beneath it.
- **The composed array must be the array its metadata describes.** The
  composition engine measures the device from the members it was handed; the
  identity records what the array was created as. A disagreement means these
  disks are not that array, so it is refused rather than published at whatever
  size the disks happen to have — publishing a device shorter than the one a
  filesystem was made on would leave every address past the end silently
  unreachable.

A member the authoritative shape does not place — one contradicting the array's
width, or losing a slot contest to a fresher copy — is held unused rather than
refused: a later, fresher member can legitimately redefine the array, and
refusing would let one corrupt disk evict a healthy one from consideration. A
refused assembly escalates the shared `RetryState` instead of being retried at
once, every wait is a deadline strictly in the future, and every table grows
fallibly, so there is no member ceiling and allocation failure is a value
rather than a panic.

## Required capabilities

Five, and no more. It holds **no** MMIO, DMA, IRQ, or mount authority: it never
touches hardware directly and never mounts a filesystem.

- `CAP_IPC_ENDPOINT` — own the rendezvous the agents offer to and each composed
  array's own block-service endpoint, and issue block calls on each member's
  delegated endpoint.
- `CAP_IPC_BIND_PRIVILEGED` — the rendezvous id is **reserved**, and binding a
  reserved id needs this. It is the gate that stops an unprivileged squatter
  claiming the id first and being handed read/write authority over every array
  member on the machine as each agent delegates to it in turn. It buys nothing
  else: the per-array endpoints are ordinary ids.
- `CAP_SHM` — map each member's delegated data window and create each array's
  own, the buffers every block transfer is staged through.
- `CAP_HW_EMIT` — publish the composed array as a storage node so the volume
  manager finds it. The node can only forward transport the composer itself
  created or was granted, so the emission widens no one's authority.
- `CAP_LOG_EMIT` — record each admission, refusal, publication, fenced start,
  maintenance failure, and change in an array's health.

It is deliberately its own bundle, separate from the sibling member agent: one
signed bundle grants its whole manifest's capability set to every instance
loaded from it, and the agent runs once per member disk, so a shared bundle
would hand every per-disk agent this driver's privileged-bind and node-emit
authority it has no need of.

## Self-maintenance

Each array heals itself under the shared `ArrayMaintenance` scheduler
(`docs/src/lib/raid.md`), which decides turn by turn what to do and when to do
nothing so the workload keeps the array. The composer only turns those decisions
into transfers:

- **Bounded turns.** One chunk per turn, staged through a single 64 KiB buffer
  shared by every array — used a whole number of array blocks at a time, so one
  buffer serves any block size. The chunk bounds how much of an array a turn
  touches and is deliberately independent of array size: a bigger array takes
  more turns, never a bigger buffer.
- **Paced against the workload.** Every request served tells the scheduler the
  array is in demand, so maintenance holds to its duty share of a busy array and
  runs flat out only on an idle one.
- **Durable position.** An advancing pass writes its position into every
  *current* member's maintenance record, so a scrub or rebuild measured in days
  survives a restart instead of beginning again. The record goes only to current
  members, so it can never claim a generation newer than the member it sits on.
  At assembly the freshest record among the array's own members is read back and
  restored into the composed device before it serves a byte; a record that is
  missing, foreign, corrupt, or from another generation simply yields no
  position, which costs a pass from the beginning and never correctness.
- **Observable.** An array losing redundancy, rebuilding, and becoming whole is
  recorded in the shared block-health vocabulary every layer uses, so it reads
  the same as a leaf disk doing the same; losing an array outright, a failed
  maintenance turn, a resumed pass, and a verification pass completing each get
  their own record.

## Limitations

- A member is never released once admitted, so a device that vanishes leaves
  its slot held until the composer restarts.
- Arrays are discovered from member metadata only. There is no creation or
  administration surface here; an array is created by writing its members'
  superblocks.

## Runtime load and unload

Loadable and unloadable at runtime like any other bundle. Unloading tears down
the rendezvous, which cancels every parked membership and wakes each agent to
re-offer, so a restarted composer reassembles every array without a reboot.

## Tests

The composer's decisions are proven host-side over member doubles
(`src/compose/tests.rs`): a complete array is composed with no wait at all
while an incomplete one settles first; the settle window is read from the
members' own declared classes and widens to the slowest of them without
restarting; an array its members cannot serve is never brought online; composing
one array marks only its own members; a member that turns up late joins the
array already serving as the stale rebuild target it is; a stale claimant of an
occupied slot and a member that disagrees about the array's shape are both held
unused rather than refused; a refused assembly backs off and escalates; and
releasing the last member forgets the array.

The live half is proven the same way (`src/service/tests.rs`): a member's
metadata is read back from its own first block; a device that cannot report its
geometry is neither read nor written; a block size that cannot stage the record
fails closed; a device with no valid metadata is refused rather than guessed at;
a complete array starts clean and leaves every member's metadata alone; a
degraded start records a new generation on every surviving member; a survivor
that cannot be re-stamped fails the whole bring-up closed; a member the metadata
proved stale joins as a rebuild target; an array its members cannot serve is
never composed; a present slot whose device cannot be supplied, a member with
nothing past its metadata, and an array that is not the size its own metadata
records are each refused; a member's reserved metadata is never reachable as
array data; a member of another array is never drawn into this one; an array
whose members recorded nothing is verified rather than assumed clean; the
freshest record among the members is the one resumed from; a record belonging to
another array is never resumed from at all, so a recycled disk can neither
inject a position nor talk this array out of verifying itself; a recorded cursor
the array will not accept is dropped rather than refusing the array; and a copy
already recorded as behind does not move the array's generation again.

The live array is proven the same way (`src/runtime/tests.rs`): it answers block
requests through the shared serve engine, refuses a malformed one without
touching the array's health, reports the ids it was published on, and places a
returning member past its own metadata exactly once while refusing a slot
outside the array or a device too small for its own metadata. Its maintenance is
proven end to end against real member disks: an array verified recently enough
asks for no work at all yet still says when to look again; an array of unknown
history verifies itself and the completed pass reaches its members' records; a
rebuild records its position on current members only, leaving the copy being
rebuilt untouched; a rebuild interrupted by a restart resumes from the recorded
position rather than starting over, with the array's generation left alone so
the record stays valid; a finished rebuild is recorded so the next start finds
the copy current; a position the members refuse is reported and still owed; and
an array that regains a copy reports rebuilding and then whole, once each.

The reassembly, escalation and composition arithmetic underneath is proven once
in `lib/raidmeta` and `lib/raid`.
