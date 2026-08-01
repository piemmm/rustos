# RAID composition

A RAID volume is a **virtual block device that composes child block
endpoints** through the same fault-aware block seam every leaf device uses
(`plans/FIX-IO.md` IO6; `AGENTS.md` §2.2, §27). It is itself a
[`tairix_abi::driver::block::Block`](../drivers/block.md), so a composed array presents
one logical device to the filesystem layer and multi-layered sets nest
naturally over the recursive seam. RAID **consumes** the block-layer health
vocabulary (`blkio::BlkStatus`, `DriverError`); it never re-invents it.

The composition engine and the maintenance policy that decides when an array
heals itself live in the shared `lib/raid` crate, over the on-disk metadata
layer in `lib/raidmeta`. They sit in `lib/` rather than in a driver because
composition is device-agnostic arithmetic over the generic block seam — the
engines compose devices they are *handed* as `Block` implementations and never
reach hardware themselves — and because two independent consumers compose
several devices as one and must share a single definition: the autoloaded RAID
composer driver (`drivers/storage/raid`) and the native filesystem's
multi-device volumes (`drivers/filesystem/arxfs`). Neither could reach a copy
held by the other, since one driver crate may not depend on another.

The autoloaded serve process that reads each discovered device's superblock,
assembles the members, and publishes the composed device as its own
block-service node is the RAID array-composer driver
(`drivers/storage/raid`), described below; turning the maintenance decisions
into real scheduled transfers is the stage that follows it
(`plans/FIX-IO.md` §2.6). The engine, its metadata, and the policy are proven
host-side first, as the other FIX-IO primitives were.

## How a device reaches the composer

An array is several block devices driven as one, so one process must hold
client authority over every member at once. Nothing is born that way: a driver
is spawned for exactly one matched hardware-tree node and receives exactly that
node's resource grants. Authority therefore flows to the composer one device at
a time, from the process that legitimately holds it.

1. **Discovery recognises the member.** While probing a device the volume
   manager reads a valid array superblock at an extent's first block
   (`lib/fsprobe`), refuses to attach it as a standalone volume — mounting one
   bare mirror copy would diverge the array or serve stale data — and publishes
   a `tairix,raid-member` node re-declaring that device's block-service
   endpoint and data window. The kernel parents the node to the volume
   manager's own matched node and admits a declared resource only if that task
   already holds a grant covering it, so the emission republishes that
   device's transport and nothing else.
2. **The member agent delegates.** The RAID member-agent driver
   (`drivers/storage/raid_member`), matched to that node, delegates the
   endpoint and the window to the composer's reserved rendezvous
   (`call_grant`, `shm_grant`) and posts a `MemberOffer` naming them
   (`tairix_abi::raid_ipc`). It is its own driver crate, not a second role of
   the composer, because one signed bundle grants its whole manifest's
   capability set to every instance loaded from it, and one instance of the
   agent runs per member disk: sharing a bundle with the composer would hand
   every agent the composer's privileged-endpoint-bind and node-emit
   authority it has no need of.
3. **The composer verifies for itself.** Which array the device belongs to,
   which slot it holds and which generation it last saw are read back off the
   device through `lib/raidmeta` — never taken from the offer. The node is a
   pointer to look, never a datum to believe, so a mistaken or malicious
   emitter cannot place a disk into an array it has nothing to do with.
4. **The membership stays open.** The composer answers the offer only when the
   membership ends, so one outstanding call carries the whole lifecycle: the
   agent parks on the reply, and the composer's endpoint being torn down
   cancels the call and wakes it, whereupon it re-offers. A composer that
   restarts reassembles its arrays without a reboot, and nothing polls.

The rendezvous id is reserved, so binding it demands
`CAP_IPC_BIND_PRIVILEGED`. That gate is load-bearing: an unprivileged squatter
that claimed the id first would be handed read/write authority over every array
member on the machine as each agent delegated to it in turn.

## Deciding when an array may be brought online (`MemberRegistry`)

Members arrive one at a time and in any order, so the composer has to judge,
each time its picture changes, whether an array is ready to serve. That
judgement is where the data-integrity risk lives — not in the IPC around it —
so it is pure logic in the RAID composer driver's library half
(`drivers/storage/raid`, `MemberRegistry`), driven by a caller-supplied
monotonic clock and proven host-side over member doubles. The live half reads a
device's superblock, hands it to the registry, and does what
`MemberRegistry::next_action` says: assemble an array, place a late member into
one already serving, or park until a deadline.

Two failures are possible, and both lose data:

- **Serving an array that cannot answer for itself.** A stripe missing a
  member, or a RAID5 missing two, has holes no redundancy can fill; publishing
  it would hand a filesystem a device that silently cannot read parts of
  itself. `RaidLevel::can_serve` (`lib/raidmeta`, beside `is_redundant` and
  `data_members`) is the single definition of that question over a reassembled
  slot table — every member for a stripe, any one copy for a mirror, one, two,
  or three losses for the parity levels, and no *pair* wholly lost for RAID10 —
  and an array that fails it is left unassembled rather than brought online
  short. It answers about the slot table; each engine's own `assemble` remains
  the authority on what the live devices can do, and the two questions compose.
- **Starting degraded too eagerly.** A member that is merely slow — spinning
  up, or riding out a bus blip inside its own driver's recovery grace window —
  is not a missing member, and bringing the array up without it forces a
  needless rebuild of a disk that was never really absent. An *incomplete*
  array therefore waits a **settle window** before it starts degraded, while a
  complete one is composed with no delay at all, so an array comes up at boot
  as promptly as a plain disk.

The settle window is not a number chosen on a developer's machine: it is the
array's own hardware's recovery grace window, taken through
`RetryCadence::for_class` and folded over the members' declared classes with
`BlkDeviceClass::most_patient`. A rotational array waits out a spin-up; a
solid-state one does not; a mixed array is only as impatient as its slowest
member. The window always runs from the instant the array's *first* member
appeared, so widening it for slow hardware can never be turned into an
indefinite postponement by a trickle of arrivals.

Everything else follows from reading the disks rather than believing the
offers:

- A member whose superblock contradicts the authoritative shape, or which loses
  a slot contest to a fresher copy, is **held unused rather than refused**. A
  later, fresher member can legitimately redefine the array and make it
  placeable, and refusing would let one corrupt or hostile disk evict a healthy
  one from consideration.
- A member that turns up after the array started degraded is offered for
  placement into the live array as the in-sync or stale copy its own generation
  counter says it is — never into a slot a serving member already holds.
- A refused assembly attempt escalates the same `RetryState` the rest of the
  RAID layer uses rather than being retried at once, so an array whose devices
  are unreachable is not re-probed in a tight loop.
- Every wait the registry asks for is an absolute deadline strictly in the
  future, and an array nothing but a further member could help asks for no
  deadline at all, so the caller always parks on a one-shot timer and never
  spins (`AGENTS.md` §2.23).
- The member table grows fallibly (`try_reserve`): there is no member ceiling
  (`AGENTS.md` §24.1) and allocation failure is a value the caller ends the
  membership on, never a panic (§2.9).

## Bringing an array online and serving it

The other pure half of the composer driver (`drivers/storage/raid`,
`service.rs`) turns those decisions into a live device. It is generic over the
member `Block` type and takes its clock as a value, so it too is proven
host-side over member doubles; the `Run` program supplies the real block
clients, the syscalls, and the audit trail.

`assemble_array` resolves the slot table, refuses anything
`RaidLevel::can_serve` rejects, and builds the level's `OwnedRaidArray` through
the shared engines. Three invariants govern it.

- **A degraded start re-stamps its survivors.** If any slot is absent or
  behind, the identity's generation is bumped and every surviving *current*
  member's superblock is rewritten at the new generation *before* the array is
  composed. A member that was away keeps its lower generation and therefore
  resolves as the stale rebuild target it is on return; a member that is
  already behind is left alone, so a rebuild target is never promoted by the
  act of starting without it. A re-stamp that cannot be written fails the whole
  bring-up rather than serving an array whose metadata lies about who is
  current.
- **A member's own metadata is not array data.** Every member is composed
  through a `tairix_partition::PartitionBlock` view beginning at
  `RESERVED_METADATA_BLOCKS`, so the superblock and the maintenance record sit
  below block 0 of the view and no array read or write can reach them. A device
  with no room beyond its reserved blocks is refused rather than composed as a
  zero-length member.
- **The composed array must be the array its metadata describes.** The engine
  measures the device from the members it was handed; the identity records what
  the array was created as. A disagreement means these disks are not that array,
  so it is refused rather than published at whatever size the disks happen to
  have — publishing a device shorter than the one a filesystem was made on would
  leave every address past the end silently unreachable.

Every refusal is a typed `ServiceError` and no array is ever composed short: a
superblock that does not decode, a block size that cannot hold the record, a
present slot whose device cannot be reached, an engine refusal, and allocation
failure are all values the caller fails closed on.

`ArrayRuntime` is one live array — its identity, its owning composed device,
its `BlkHealth`, and the ids of the block endpoint, shared window, and
hardware-tree node it was published on. It answers requests through the *same*
`blkio::serve_request_recovering` engine a leaf device is served with, so an
array rides the same recovery grace window a disk does, a member blip inside
that window is answered reissuably, and there is no second serve path to keep
in step.

The published node carries the `tairix,raid-array` compatible key plus the
array's endpoint and window as resources. The volume manager binds it exactly
as it binds a disk's per-LUN node, so an array's filesystems are probed and
mounted through the unmodified volume path — and because the node is
indistinguishable in kind from a disk's, an array can itself be a member of
another array with no extra machinery.

## RAID1 mirror (`MirrorArray`)

The first composition is a RAID1 mirror: every member holds a full copy of the
same logical-block array, so the array survives any subset of member faults as
long as one copy remains. The array borrows a caller-owned member slice, so it
holds no allocation and imposes no fixed member ceiling (`AGENTS.md` §24.1);
the growable member tier lives in the assembling serve process.

### Read — recover and repair

Reads are served from an in-sync member, trying members in a deterministic
order (no coin-flip). A member that returns a *per-block* error
(`DriverError::MediumError`) does not kill the array: the data is recovered
from a good copy and the bad copy is **repaired** in place by writing the good
data back, forcing the device to reallocate the sector. Only a *whole-device*
fault (`DeviceOffline`/`DeviceFault`, or a member returning a request-level
error for a request the array already validated) drops the copy from the array.
A read with no surviving copy fails closed and never fabricates data
(`AGENTS.md` §5.4).

This read-path repair is **opportunistic**: it only ever touches the copies a
read consults *before* the first that serves the block, so a latent media error
on a copy that is never chosen as the read source stays invisible until the
copies ahead of it are gone. The proactive scrub below is the complement that
finds such latent errors.

### Scrub — proactive verify and repair

`MirrorArray::begin_scrub` / `scrub_step` drive a bounded, interruptible pass
that reads **every** in-sync copy of **every** block and repairs a copy that
cannot read a block from one that can — the auto-scrub a mirror exists to
provide (`AGENTS.md` §26.5). `scrub_step` verifies one caller-sized chunk and
advances a cursor, so a 100 TB+ array is scrubbed a chunk at a time and never
in one unbounded sweep or a busy-spin (`AGENTS.md` §26.6, §2.23); a larger
scratch buffer scrubs faster, a smaller one yields sooner. `scrubbing` reports
whether a pass is still in progress and `scrub_cursor` its position (for
progress logging).

Within a chunk a copy that reads cleanly is verified good; a *whole-device*
fault drops the copy exactly as on the read path; and a *per-block* media error
is repaired by writing back data read from a good copy (a repair whose
write-back fails drops that copy, but the data is safe on the source, so it is
not a loss). If a block is bad on **every** copy the loss is surfaced as a
typed error, but the cursor still advances past it so a repeated call makes
progress rather than looping on the unrepairable block; the bad block is left
for the read path to surface. A scrub on a failed array (no in-sync copy) fails
closed without advancing.

A scrub deliberately does **not** arbitrate a *content* disagreement between
two copies that both read cleanly: a bare mirror has no authority to decide
which differing copy is correct, and overwriting one from another could
propagate corruption. Detecting silent divergence is the checksummed
filesystem layer's job (ARXFS), not the block mirror's; the scrub's remit is
latent *media* errors. Scrub buffers hold opaque on-disk bytes that may include
secrets, so they are staged as `BufferClass::Sensitive`.

### Write — fan out and drop

Writes fan out to every copy. A member that fails a write is dropped
immediately (a write error is a member fault); the write still succeeds as long
as one copy accepted it, and fails closed only when none did. A member being
rebuilt receives writes to its already-synced region so it never falls behind
the source.

### Degrade — never fail the system

A member going faulted degrades the array, never the system: the survivors
keep serving and the array reports `ArrayHealth::Degraded`. A flush commits
every copy and drops any that cannot, keeping at least one durable copy or
failing closed.

A **missing member** — a slot the array is defined to have but which holds no
device — is a first-class `MemberState::Absent`, the equivalent of a Linux md
"removed" slot. `MirrorArray::assemble` is given the array's *full* member
table, one `MirrorMember::absent()` per missing copy, so the assembled array
counts the empty slot toward its member count and reports `Degraded` for the
reduced redundancy rather than silently presenting as a smaller, optimal
array: a mirror short a member never masquerades as fully redundant
(`AGENTS.md` §26.5). An absent slot serves no read, receives no write, and
never fabricates a device; it is simply held open until a spare fills it.

The runtime disk-replacement workflow mirrors mdadm's remove/add:
`MirrorArray::remove_member` pulls a faulted disk, vacating its slot to
`Absent` and returning the removed device to the caller (only a faulted
member may be pulled — a live one is still participating); and
`MirrorArray::add_member` installs a fresh spare into an absent slot, which
begins `Resyncing` and rebuilds from a surviving copy exactly as a returned
member does. Both fail closed on a bad index, and `add_member` refuses an
already-occupied slot (`SlotOccupied`) or a spare whose geometry does not
match (leaving the spare `Faulted`, never admitted as a rebuild source). The
whole cycle — fault, remove, add, rebuild — restores full redundancy without
a reboot (`AGENTS.md` §18.4).

### Rebuild — bounded, interruptible resync

A returning member (via its own recovery grace window, `plans/FIX-IO.md` IO3)
or a physically replaced disk is rebuilt by a bounded, interruptible resync:
`MirrorArray::resync_step` copies the array contents from an in-sync member a
caller-sized chunk at a time, so a 100 TB+ member rebuild never blocks the
system or busy-spins (`AGENTS.md` §26.6). A rebuilding member becomes a read
source only once fully in sync; while rebuilding the array reports
`ArrayHealth::Recovering`. Array health maps onto the shared mount-availability
vocabulary (`MountAvailability::Available`/`Degraded`/`Recovering`/
`UnavailableLost`) so a serving process surfaces it through the same `sysinfo`
mount surface a leaf volume uses (`plans/FIX-IO.md` IO2/IO5), never a second
vocabulary.

## RAID0 stripe (`StripeArray`)

The second composition is a RAID0 stripe. It is a **sibling** of the mirror
over the same block seam (`AGENTS.md` §2.2 parallel implementations), not a
variant of it: the logical block space is cut into fixed-size *chunks*
(`chunk_blocks` logical blocks each) and round-robined across the members, so
the array's capacity is the **sum** of the members' and a large transfer is
spread over every member. Like the mirror it borrows a caller-owned member
slice, so it holds no allocation and imposes no fixed member ceiling
(`AGENTS.md` §24.1). It shares the mirror's whole-device-fault classification
(`member_faulting`) and the `ArrayHealth` vocabulary rather than re-inventing
them (`AGENTS.md` §2.2).

### No redundancy — fail closed, never degrade

A stripe holds exactly one copy of each block, so it has **no redundancy**:
losing any one member loses a fraction of every stored object. The engine is
honest about this rather than pretending otherwise (`AGENTS.md` §5.4, §26.5),
and that shapes every behaviour:

- **Assembly requires every member present and evenly striped.** Unlike a
  mirror, a stripe cannot come up "degraded" over a missing or unwell member —
  there is no other copy to serve the blocks that member holds.
  `StripeArray::assemble` probes every member and fails closed if any cannot
  report geometry (`StripeError::MemberUnavailable`), if the members disagree
  on geometry (`GeometryMismatch`), if a member reports a degenerate geometry
  (`ZeroGeometry`), if a member's size is not a whole number of stripe chunks
  (`UnalignedGeometry`, which would leave a ragged tail), if the stripe unit is
  zero (`ZeroChunk`), if there are no members (`NoMembers`), or if the summed
  block count overflows `u64` (`TooLarge`). The composed array presents the
  members' shared block size and the sum of their block counts.
- **A whole-device fault fails the array closed for good.** When a member
  returns a whole-device fault (gone/removed/unrecoverable), the stripe is
  marked `ArrayHealth::Failed` and *every* subsequent read, write, and flush
  fails closed (`DriverError::DeviceOffline`) — the array can no longer present
  a complete logical block space, and it never pretends it can. This is sticky:
  a stripe has no way to rebuild a lost member.
- **A per-block media error fails only that request.** A bad sector on a
  member (`DriverError::MediumError`) means that one logical block is
  unrecoverable (no second copy to heal from), so the affected request fails
  closed — but the device is still reachable, so the array stays
  `ArrayHealth::Optimal` and unrelated stripes keep serving.

A stripe therefore only ever reports `ArrayHealth::Optimal` or
`ArrayHealth::Failed`; it has no `Degraded`/`Recovering` state of its own,
because it has nothing to degrade *to* and nothing to rebuild *from*. It maps
onto the shared mount-availability vocabulary through the same
`ArrayHealth::to_mount_availability` a leaf volume and the mirror use.

### Striping layout

Logical block `b` sits on member `(b / chunk_blocks) % member_count` at that
member's local block `((b / chunk_blocks) / member_count) * chunk_blocks + b %
chunk_blocks`. A read or write is split at chunk boundaries and each contiguous
run dispatched to the member that holds it, so one logical transfer scatters or
gathers across the members. All index arithmetic is bounds-checked and the
narrowing to a slot index fails closed rather than panicking (`AGENTS.md`
§2.9). A flush commits **every** member, because each holds a disjoint slice of
the block space and durability requires all of them; a member that cannot
flush fails the whole flush closed (`AGENTS.md` §5.4).

## RAID5 distributed parity (`ParityArray`)

The third composition is a RAID5 distributed-parity array: it combines the
stripe's capacity aggregation with single-fault redundancy. The logical block
space is striped in fixed-size chunks across the members, and each stripe
reserves one member's chunk for the parity (bytewise XOR) of the other `n - 1`
data chunks. The parity slot **rotates** one member per stripe (left-symmetric
placement), so no single member is a parity write bottleneck — the RAID4→RAID5
distinction. The array's usable capacity is that of `member_count - 1` members,
and a RAID5 array needs at least three members. Like its siblings it borrows a
caller-owned member slice (no fixed member ceiling, `AGENTS.md` §24.1); because
parity computation and reconstruction need a working buffer the `Block`
read/write methods do not carry, it also borrows a caller-owned **scratch**
buffer of at least two logical blocks.

### Layout

For a stripe `s` of `n` members the parity member is `p = (n - 1) - (s mod n)`,
and the `n - 1` data chunks are placed on the non-parity members in ascending
order starting just after `p`: data position `k` sits on member
`(p + 1 + k) mod n`. Every member's chunk of a given stripe lives at the same
member-local LBA, so the XOR of all members' blocks at any member-local LBA is
zero — the invariant that makes reconstruction uniform for a data chunk and a
parity chunk alike.

### Read — direct, reconstruct, and repair

A healthy read goes straight to the data member holding the block. A read of a
block on a lost member is **reconstructed** by XOR-ing the same offset from
every surviving member (data and parity). A *per-block* media error on an
otherwise-healthy member is reconstructed from the survivors and **repaired**
in place (forcing sector reallocation), exactly as the mirror does. A read that
would need two members it cannot get fails closed (`AGENTS.md` §5.4) rather than
fabricate data.

### Write — read-modify-write and degraded parity

A write updates the affected stripe's parity. When the old data and old parity
are both readable it uses read-modify-write
(`new_parity = old_parity XOR old_data XOR new_data`), the 2-read/2-write path
that is independent of member count. When they are not — the data member is
lost, or its old data hit a media error — it recomputes the parity from the
surviving data members (`new_parity = new_data XOR other data`), so a lost
member's data stays reconstructable. If the parity member itself is lost the
data is written directly and the parity is rebuilt later. A resyncing member's
already-rebuilt region is kept current so it never falls behind mid-rebuild.

The caller's `BufferClass` is forwarded to the write of the caller's own data
block, exactly as the mirror and stripe do, so a `Sensitive` write is zeroed on
free and a `NonSensitive` bulk write is not needlessly slowed. The parity write
and every read-modify-write / reconstruction staging buffer stay
`BufferClass::Sensitive` regardless, because they mix other stripes' opaque
on-disk bytes; only the caller's own data write carries the caller's class.

### Scrub — proactive verify and repair

`ParityArray::begin_scrub` / `scrub_step` drive a bounded, interruptible pass
that reads every in-sync member's copy of every stripe row and repairs a latent
media error from the survivors (`AGENTS.md` §26.5), chunked so a 100 TB+ array
never scrubs in one sweep or a busy-spin (`AGENTS.md` §26.6, §2.23). Like the
mirror, a parity scrub heals latent *media* errors; it does **not** arbitrate a
parity *content* disagreement (a bare parity array cannot know which member is
wrong — that is the checksummed filesystem layer's job).

### Degrade, rebuild, and replace

A faulted member, or a missing slot (`MemberState::Absent`), degrades the array
(`ArrayHealth::Degraded`) while the survivors keep serving; a *second* loss
makes a stripe unrecoverable and the array fails closed
(`ArrayHealth::Failed`). A returning or physically replaced member is rebuilt
by `ParityArray::resync_step`, which reconstructs its blocks from the survivors
a caller-sized budget of blocks at a time (`AGENTS.md` §26.6), becoming a read
source only once fully in sync (`ArrayHealth::Recovering` meanwhile). The
disk-replacement workflow mirrors the RAID1 one — `remove_member` vacates a
faulted slot to `Absent` (returning the device), `add_member` installs a spare
into an absent slot, and `replace_member` hot-swaps a faulted one — each
rebuilding from the survivors and restoring redundancy without a reboot
(`AGENTS.md` §18.4). A faulted member is sticky-but-recoverable, so a flapping
disk never masquerades as a healthy copy.

## RAID6 double distributed parity (`DualParityArray`)

`DualParityArray` composes `member_count - 2` members' worth of capacity as one
logical device with **double-fault** redundancy — the sibling of the mirror,
the stripe, and the single-parity array over the same block seam (`AGENTS.md`
§2.2 parallel implementations), reusing their `MemberState`/`MemberRole`/
`ArrayHealth` vocabulary and `member_faulting` classification. It needs at least
four members (two data + P + Q).

### Layout — left-symmetric double parity over GF(2^8)

Each stripe reserves *two* chunks: a **P** syndrome (bytewise XOR of the data
chunks, exactly RAID5's parity) and a **Q** syndrome, the Reed-Solomon sum
`Q = Σ gᵏ·Dₖ` over the finite field GF(2^8) (`lib/*` `gf256`, generator `{02}`,
reducing polynomial `0x11d` — the same field the Linux RAID6 implementation
uses, so the layout is a well-understood one). For a stripe `s` the P member is
`p = (n - 1) - (s mod n)` and the Q member is `q = (p + 1) mod n`; the `n - 2`
data chunks fill the remaining members in ascending order after `q`, with data
position `k` carrying Q coefficient `gᵏ`. Both syndrome slots rotate one member
per stripe so neither is a bottleneck. Because the generator's powers `g⁰ …
g²⁵⁴` are the 255 distinct non-zero field elements, an array admits at most 255
data members; `assemble` fails closed (`DualParityError::TooManyMembers`) above
that rather than encode an unrecoverable Q.

### Read — direct, reconstruct (one or two losses), and repair

A healthy read goes straight to the data member. When members are lost, the
stripe row's *unknowns* (every not-in-sync member, plus a data member that hit a
media error) are solved from the surviving members and syndromes: a single
unknown data chunk is recovered from P (like RAID5); two unknowns are solved
from the two independent P and Q equations — through Q when P is also lost,
through P when Q is also lost, or by the 2×2 syndrome system when two *data*
chunks are lost. A per-block media error is reconstructed and repaired in place
by writing the good block back (forcing sector reallocation). A row with a
*third* unknown is unsolvable and fails closed (`AGENTS.md` §5.4). All
reconstruction is byte-wise and borrows a caller-owned scratch buffer of at
least `SCRATCH_BLOCKS` logical blocks, so the engine allocates nothing.

### Write — read-modify-write and degraded recompute

A single-block write updates both syndromes. When the data member and both
syndromes are readable, a read-modify-write applies the data delta to P
(`new_P = old_P ⊕ Δ`) and to Q (`new_Q = old_Q ⊕ gˣ·Δ`). Otherwise (a lost or
media-erroring role) both syndromes are recomputed from every data member's
current content — reconstructing any lost data member — with the written
position substituted. Each stripe role that is a live source is written with its
new value; a write that fails faults that member (excluding its stale block),
and the new data stays durable while the array keeps its two-fault redundancy.
As with RAID5, the caller's `BufferClass` is forwarded to the write of the
caller's own data block, while both P and Q syndrome writes and every staging
buffer stay `BufferClass::Sensitive` because they carry opaque cross-stripe
bytes.

### Scrub, degrade, rebuild, and replace

`begin_scrub` / `scrub_step` heal latent media errors from the survivors like
the single-parity array, chunked so a 100 TB+ array never scrubs in one sweep
(`AGENTS.md` §26.5, §26.6). One or two faulted/absent members degrade the array
(`ArrayHealth::Degraded`); a *third* loss fails it closed (`ArrayHealth::
Failed`). A returning or replaced member is rebuilt by `resync_step` from the
survivors a caller-sized budget at a time, and the same `remove_member` /
`add_member` / `replace_member` disk-replacement workflow restores redundancy
without a reboot (`AGENTS.md` §18.4). Faulted members are
sticky-but-recoverable, so a flapping disk never masquerades as a healthy copy.

## RAID-TP triple distributed parity (`TripleParityArray`)

`TripleParityArray` composes `member_count - 3` members' worth of capacity as
one logical device with **triple-fault** redundancy — the sibling of the
mirror, the stripe, and the single- and double-parity arrays over the same
block seam (`AGENTS.md` §2.2 parallel implementations), reusing their
`MemberState`/`MemberRole`/`ArrayHealth` vocabulary and `member_faulting`
classification. It needs at least five members (two data + P + Q + R).

### Layout — left-symmetric triple parity over GF(2^8)

Each stripe reserves *three* chunks: a **P** syndrome (bytewise XOR of the data
chunks), a **Q** syndrome (`Q = Σ gᵏ·Dₖ`), and an **R** syndrome
(`R = Σ g²ᵏ·Dₖ`), all over the finite field GF(2^8) (`lib/*` `gf256`, generator
`{02}`, reducing polynomial `0x11d` — the same field the Linux RAID6/RAID-Z
implementations use). For a stripe `s` the P member is `p = (n - 1) - (s mod n)`,
the Q member is `q = (p + 1) mod n`, and the R member is `r = (q + 1) mod n`;
the `n - 3` data chunks fill the remaining members in ascending order after `r`,
with data position `k` carrying Q coefficient `gᵏ` and R coefficient `g²ᵏ`. All
three syndrome slots rotate one member per stripe so none is a bottleneck. As
for double parity the generator's powers stay distinct for at most 255 data
members; `assemble` fails closed (`TripleParityError::TooManyMembers`) above
that (over 258 slots).

### Read — direct, reconstruct (up to three losses), and repair

A healthy read goes straight to the data member. When members are lost, the
stripe row's *unknowns* (every not-in-sync member, plus a data member that hit a
media error) are solved from the survivors: the unknown *data* chunks are the
solution of the surviving syndromes' **Vandermonde system** (the coefficient
rows `(1, gᵏ, g²ᵏ)` over the distinct nodes `gᵏ` are always invertible for up to
three unknowns), and any unknown *syndrome* is then recomputed from the
now-known data. The per-byte matrix inverse is computed once per stripe row and
applied byte-wise, so reconstruction is `O(bytes)` after a fixed setup. A
per-block media error is reconstructed and repaired in place (forcing sector
reallocation). A row with a *fourth* unknown is unsolvable and fails closed
(`AGENTS.md` §5.4). All reconstruction borrows a caller-owned scratch buffer of
at least `SCRATCH_BLOCKS` logical blocks, so the engine allocates nothing.

### Write — read-modify-write and degraded recompute

A single-block write updates all three syndromes. When the data member and all
three syndromes are readable, a read-modify-write applies the data delta:
`new_P = old_P ⊕ Δ`, `new_Q = old_Q ⊕ gˣ·Δ`, `new_R = old_R ⊕ g²ˣ·Δ`.
Otherwise all three are recomputed from every data member's current content —
reconstructing any lost data member — with the written position substituted.
Each stripe role that is a live source is written; a write that fails faults
that member, and the new data stays durable while the array keeps its
three-fault redundancy. As with the other parity levels the caller's
`BufferClass` is forwarded to the caller's own data block, while the P, Q, and R
syndrome writes and every staging buffer stay `BufferClass::Sensitive`.

### Scrub, degrade, rebuild, and replace

`begin_scrub` / `scrub_step` heal latent media errors from the survivors,
chunked so a 100 TB+ array never scrubs in one sweep (`AGENTS.md` §26.5, §26.6).
One, two, or three faulted/absent members degrade the array
(`ArrayHealth::Degraded`); a *fourth* loss fails it closed
(`ArrayHealth::Failed`). A returning or replaced member is rebuilt by
`resync_step` from the survivors a caller-sized budget at a time, and the same
`remove_member` / `add_member` / `replace_member` disk-replacement workflow
restores redundancy without a reboot (`AGENTS.md` §18.4). Faulted members are
sticky-but-recoverable, so a flapping disk never masquerades as a healthy copy.

## RAID10 stripe of mirrors (`Raid10Array`)

`Raid10Array` composes an **even** number of members (at least four) into
two-copy mirror **pairs** and stripes the logical block space in fixed-size
chunks across the pairs — a stripe *of* mirrors. It combines mirror redundancy
with stripe capacity and bandwidth: the array presents half its members' worth
of capacity and survives any member fault — and several at once — as long as no
mirror pair loses *both* copies. Like its siblings it borrows a caller-owned
member slice and holds no allocation (`AGENTS.md` §24.1).

### It is a composition, not a re-implementation

A stripe of mirrors *is* a stripe over mirrors, so the engine composes the two
it is built from rather than copying their logic (`AGENTS.md` §2.2):

- the **striping map** (`StripeArray::locate`, shared `pub(crate)`) places each
  logical chunk on the pair (column) that holds it, exactly as RAID0 does
  across members;
- each **mirror pair** is driven through the one `MirrorArray` implementation —
  recover-from-a-good-copy, opportunistic read-repair, write fan-out, scrub,
  and bounded rebuild — by building a transient `MirrorArray::from_prepared`
  view over the pair's two members per operation (an allocation-free borrow).

So RAID10 adds only the *pairing* and the *aggregation of per-pair health into
array health*; every fault-recovery behaviour is the mirror's, verified once in
the mirror's own tests.

### Fault model, scrub, rebuild, and replace

`assemble` probes each pair through `MirrorArray::assemble` (reusing the
mirror's probing and geometry rules) and requires every present member to agree
on geometry and to be a whole number of stripe chunks; an odd member count
(`Raid10Error::OddMembers`) or fewer than four (`TooFewMembers`) is refused.
A pair with one copy down is `ArrayHealth::Degraded` (or `Recovering` while
that copy rebuilds) but keeps serving from the survivor through the mirror's
recover-and-repair path; a pair that loses *both* copies can no longer serve
its stripes, so the whole array is `ArrayHealth::Failed` and that region fails
closed (`DeviceOffline`, `AGENTS.md` §5.4) while the *other* pairs keep serving
(head-of-line freedom, §26.1). The array is `Optimal` only when every pair
holds two in-sync copies.

`begin_scrub` / `scrub_step` drive one shared member-local cursor across every
pair, healing latent media errors chunk by chunk (`AGENTS.md` §26.5, §26.6);
`resync_step` rebuilds each pair's resyncing copy from its survivor; and the
`readd_member` / `remove_member` / `add_member` / `replace_member` cycle maps a
global slot to its pair and drives the mirror's own disk-replacement workflow,
restoring redundancy without a reboot (`AGENTS.md` §18.4).

## On-disk metadata and reassembly (`ArraySuperblock`, `ArrayIdentity`)

The on-disk metadata format and the reassembly logic live in the shared
`lib/raidmeta` crate, not in a driver, so the RAID composition engines here
and the **storage-discovery probe** (`lib/fsprobe`, used by
`drivers/storage/volmgr`) read one definition of what a member is and can never
disagree (`AGENTS.md` §2.2) — and without a `drivers/*`→`drivers/*` edge
(§17.4). This crate re-exports the types (`tairix_raid::ArraySuperblock`,
…) for its own use. The probe consumes the same decode to **refuse mounting a
bare, un-assembled member**: a member carries its superblock at block 0, so
`fsprobe::probe_raid_member` recognises it before any filesystem signature and
the volume manager skips it rather than attaching one raw mirror copy
read-write (which would diverge the array or serve stale data, `AGENTS.md`
§26.5).

An array is **discovered, not configured**: there is no hand-maintained list
of which devices form an array (`AGENTS.md` §18, §16.5). Each member carries a
fixed-size, little-endian `ArraySuperblock` naming the array (a 128-bit
`ArrayUuid`), the RAID level, the total member count, this member's slot, the
array geometry, a monotonic **generation** counter, a `Time64` last-write
stamp, and the stripe unit (`chunk_blocks`) for a striped level. The level and
the stripe unit must agree: a striped level (RAID0) records a non-zero
`chunk_blocks`, a full-copy level (the mirror) records zero, and a record whose
level and stripe unit contradict is refused (`SuperblockError::BadStripeChunk`)
so a corrupt or foreign record is never mistaken for a valid array. The member
count must likewise be one its level can actually be composed from
(`RaidLevel::min_members`/`RaidLevel::max_members`, the single shared
definition the composition engines also read): a RAID5 claiming two members, a
RAID6 claiming three, a RAID-TP claiming four, a RAID10 claiming an *odd*
count (its copies cannot pair) or fewer than four, or a GF(2^8) parity level
claiming more data members than its syndromes can distinguish (more than 255,
i.e. over 257 slots for RAID6 or 258 for RAID-TP) describes an array that
cannot exist and is refused (`SuperblockError::MemberCountOutOfRange`) rather
than half-trusted. Each level's *usable capacity* lives beside those bounds as
the same single definition — `RaidLevel::data_members` (a stripe concatenates
every member, a mirror presents one copy, single parity reserves one member,
double parity two, triple parity three, and RAID10 presents half its members)
and the
`RaidLevel::logical_block_count` that
sizes the composed geometry as `per_member_blocks × data_members`, failing
closed on an overflow that would truncate addresses. The concatenating engines'
`assemble` derive the array block count from that one rule, so the capacity a
serving process presents and the geometry each engine composes cannot drift
(`AGENTS.md` §2.2). The record
is sealed with a trailing CRC-32C (`lib/crc32c`, the one
first-party checksum) — a media/transport integrity check, not a security
control: an array's authenticity rests on the signed driver bundle and the
members' own capability-gated block endpoints, not on this value.

`ArraySuperblock::decode` **fails closed** on any malformed on-disk byte
(`AGENTS.md` §5.4, §26.5) — a bad magic, an unknown version, a checksum
mismatch, an unknown RAID level, a zero member count, a member count the level
cannot be composed from, a slot outside the array, a degenerate geometry, or a
non-canonical timestamp is a typed `SuperblockError`, never a silently-trusted
record. (`chunk_blocks` is part of
the array shape `resolve`/`verdict_of` compare, so a member disagreeing on the
stripe unit is refused like any other shape mismatch.) The decoder is total and
`forbid(unsafe_code)`, and a fuzz harness (`lib/raidmeta/tests/fuzz_superblock.rs`,
`AGENTS.md` §19.6) proves it never panics on arbitrary input and that every
accepted record round-trips.

Discovery hands the assembler a heterogeneous set of `Candidate` members whose
superblocks decoded — some may belong to one array, some to another. The
`distinct_arrays(candidates)` iterator is the "which arrays are on these disks"
step: it enumerates the distinct `ArrayUuid`s present, each exactly once and in
first-appearance order, allocating nothing and imposing no ceiling on the number
of arrays (`AGENTS.md` §24.1), so the assembler resolves each array in turn.

Reassembly then resolves each array from the members claiming it:

- `ArrayIdentity::resolve(target_uuid, candidates)` fixes the authoritative
  array shape (level, member count, geometry) and current generation from the
  **freshest** matching member — the one reporting the highest generation.
  Trusting the freshest member is the standard RAID rule (mdadm's event
  count): a member that missed a membership change is behind, so a survivor
  that stayed live is the source of truth. It fails closed with
  `AssemblyError::NoMembers` if no candidate belongs to the target array.
- `ArrayIdentity::verdict_of` is the single per-member decision: a member
  whose generation matches the authoritative one is placed **in sync**; a
  member that is behind is placed **stale** (a rebuild target `assemble`
  brings up `Resyncing`, never a read source); a foreign array, a member
  disagreeing on the array shape, an out-of-range slot, or a duplicate claim
  on a slot (the fresher — or, on a tie, lower-tagged — copy wins) is refused
  and never admitted, so a corrupt or clone disk cannot poison the array.
- `ArrayIdentity::fill_slots` builds the whole slot table from that one
  decision, so a per-member verdict and the assembled table can never disagree
  (`AGENTS.md` §2.2). Both are pure and allocation-free — the caller owns the
  candidate slice and the slot buffer, so there is no fixed member ceiling
  (`AGENTS.md` §24.1).

The reassembly verdict is carried into the composition through **one** mapping,
`MemberRole::for_slot`, so the metadata layer and the mirror cannot disagree on
what "in sync" means (`AGENTS.md` §2.2): a `SlotDisposition::Missing` slot
offers no device (`None`); a `Present { in_sync: true }` slot becomes a
`MemberRole::Current` member; a `Present { in_sync: false }` slot becomes a
`MemberRole::Stale` member. `MirrorArray::assemble` turns a member's role into
its initial state — a current copy that probes cleanly is `InSync` (a read
source at once), a stale copy that probes cleanly is `Resyncing` (rebuilt from
a current copy before it ever answers a read), and any copy that cannot be
probed is `Faulted`. A copy the generation counter proved is behind therefore
can **never** be served to a reader as if it were current (`AGENTS.md` §5.4,
§26.5) — a disk that missed writes is a disk that can lie.

### Building the member buffer (`fill_members`)

`MemberRole::for_slot` maps *one* slot; turning the whole
`SlotDisposition` table into a redundant engine's member buffer is the shared
`fill_members` bridge, so every consumer that assembles a discovered array —
the autoloaded serve process and the ARXFS-native multi-device composition
alike — places its members through one definition instead of hand-rolling the
stale/absent/device-tag loop (`AGENTS.md` §2.2, §27). Getting that loop subtly
wrong is a data-integrity fault, not a cosmetic one: admitting a stale slot as a
trusted read source, or dropping a copy when the buffer width and the slot table
disagree, is exactly the stale-read / lost-copy hazard the metadata layer exists
to prevent (`AGENTS.md` §5.4, §26.5).

`fill_members(slots, members, take_device)` populates a caller-owned member
buffer from the reassembled slot table, placing each slot through
`MemberRole::for_slot`: a present in-sync copy joins `Current`, a present stale
copy joins `Stale` (a rebuild target, never an immediate read source), and a
missing slot becomes an absent member so the array knows its true width. It
takes each present slot's device from the caller's `take_device(tag)` supplier
(consulted once per present slot, never for a gap) and **fails closed** rather
than composing a partial array: a present slot whose device the supplier cannot
resolve is `AssembleError::MissingDevice` (never silently demoted to absent,
which would drop a copy), and a member buffer that is not the slot table's width
is `AssembleError::WidthMismatch`. The bridge is defined through the
`AssembleMember` trait over the redundant member types that carry the
current/stale/absent vocabulary (`MirrorMember` — shared by the mirror and
RAID10 — `ParityMember`, `DualParityMember`, `TripleParityMember`); the
no-redundancy RAID0 stripe is deliberately excluded, since its `assemble` fails
closed on a gap rather than composing around one, and a `StripeMember` has
neither a stale nor an absent state. The engine's own `assemble` still
re-derives each present member's real state from a live geometry probe, so this
bridge fixes only the *role* each slot joins with.

### Metadata updates (membership changes)

Reassembly *reads* the generation counter; the write side *advances* it. When
the array's membership changes — a member drops out on a fault, or a rebuilt
member rejoins — the serve process advances the generation and re-stamps the
survivors:

- `ArrayIdentity::bump_generation` returns the identity at the next generation
  (the array's event count, mdadm-style). It saturates at `u64::MAX` rather
  than wrapping, since a wrapped generation could match an already-written
  member's value; saturation is the safe direction, and `2^64` membership
  changes is unreachable in practice.
- `ArrayIdentity::member_superblock(slot, updated_at)` builds the on-disk
  record a **current** member persists: the array's shape and its current
  generation. It fails closed with `None` for a slot outside the array. This
  is the record written to a survivor re-stamped after a membership bump, to a
  freshly-created member, and to a rebuilt member promoted back to current on
  resync completion — writing the current generation is exactly what makes a
  formerly-stale copy resolve as in sync again.

A member that was **absent** for a bump is never re-stamped, so it keeps its
lower generation and returns as a stale rebuild target rather than a trusted
read source — this is what closes the stale-read window (`AGENTS.md` §5.4,
§26.5): a disk that missed writes while it was gone can never come back
masquerading as up to date. The read and write halves share one notion of
"current" (the generation equality `verdict_of` tests), so they cannot diverge
(`AGENTS.md` §2.2). A still-rebuilding member is deliberately left at its lower
generation until its resync finishes, so it stays read-excluded until it is
genuinely caught up.

The on-disk format is unfrozen pre-release (`AGENTS.md` §2.13): it is changed
in place, never versioned alongside an old one.

### States

A member slot is `InSync` (a full copy: read source and write target),
`Faulted` (dropped after a whole-device fault or a failed write; its device is
retained in the slot for a re-add, but it neither serves nor receives I/O),
`Resyncing` (being rebuilt; receives synced-region writes, not yet a read
source), or `Absent` (no device present — a missing member the array is
defined to have). The slot's device presence is exactly determined by this
state: every state but `Absent` has a backing device, and `Absent` has none;
the constructors and reconfiguration operations preserve that invariant so the
two cannot drift.

A faulted copy is sticky-but-recoverable: it stays out of the array until it
demonstrably returns through `readd_member` (re-probing its retained device)
or `replace_member` (hot-swapping in a fresh device), so a flapping disk
cannot masquerade as a healthy copy, yet a genuine return always rejoins
without a reboot (`AGENTS.md` §18.4). A faulted disk can instead be pulled
entirely with `remove_member` (slot → `Absent`) and a spare later inserted
with `add_member`; see *Degrade — never fail the system* above.

## Device health (`device_health`)

Every composition is itself a [`Block`](../drivers/block.md), so a consumer that
schedules a scrub from a device's `SMART` / `NVMe` telemetry
(`docs/src/filesystem/arxfs-spec.md` §11) queries the *array* through
`Block::device_health` and must still see the health of the disks underneath
it. Rather than inherit the trait default (`Unavailable`) — which would hide
every member's telemetry and make a failing disk in an array look like a
device with no health data at all (`AGENTS.md` §26.5) — all the compositions
aggregate their live members' snapshots through one shared definition
(`aggregate_device_health`, `AGENTS.md` §2.2), so they cannot fold health
differently.

The fold keeps a baseline comparison meaningful for an array:

- **Independent per-device integrity faults are summed** — `media_errors`,
  `reallocated_sectors`, `pending_sectors`, `uncorrectable_sectors`, and
  `crc_errors`. Each member's errors are its own, so the array total is their
  sum, and a rise in the sum is what schedules a deep scrub. Sums saturate
  rather than wrap, so a very wide array of old disks can never overflow a
  counter (`AGENTS.md` §2.9, §26.6).
- **Shared / whole-array conditions take the worst member** — `unsafe_shutdowns`
  and `power_on_hours` are the maximum (an unclean shutdown hits every member
  together, so summing would fabricate a fault), `percentage_used` and
  `temperature_kelvin` are the maximum (the most-worn / hottest member bounds
  the array), `available_spare` is the minimum (the weakest link), and
  `critical_warning` is the logical OR.

Only live, participating devices contribute: an in-sync copy and a resyncing
copy (a real device being rebuilt) report valid telemetry, while a faulted or
absent slot has none. A member that itself reports `Unavailable`, or whose
health read *errors*, is skipped rather than failing the whole array-level
query — a single member with no telemetry never denies the consumer the health
of the members that can be read (`AGENTS.md` §26.5). The array reports
`Unavailable` only when no participating member exposes telemetry, so an
absence of data is never mistaken for a perfectly-healthy array.

## Device class (`device_class`)

An array is also a device in the eyes of the consumer that derives its I/O
budget — the per-request deadline, reissue count, and recovery grace window —
from a device's declared `BlkDeviceClass` (see [block drivers](../drivers/block.md)).
An array answers only as fast as the member it is waiting on, so it declares
the **most patient** of its live members' classes
(`BlkDeviceClass::most_patient`): a mirror pairing an SSD with a spinning disk
must be given the spinning disk's spin-up budget, or a consumer would time out
a perfectly healthy array whenever the slow copy served a read. The fold is
commutative, so member order cannot change the answer, and it is derived from
the budgets themselves rather than a second hand-written ordering that could
drift from them.

The class fold sits beside the health fold as one shared definition
(`aggregate_device_class`, `AGENTS.md` §2.2) and selects members through the
*same* participation predicate, so an array can never report its health from
one set of members and its class from another. A member that faults out stops
buying the array its patience — the array is no longer waiting on it — and an
array with no live member left declares the bounded unclassified envelope
rather than the widest one, so its callers fail closed sooner instead of
waiting out disks that are not there.

## Composed-device dispatch (`RaidArray`)

The six compositions above are siblings over the same block seam (`AGENTS.md`
§2.2), but a serving process must present exactly **one** logical
[`tairix_abi::driver::block::Block`](../drivers/block.md) device to the filesystem
layer once it has *discovered* an array and resolved its `RaidLevel`,
regardless of which level composes it. `RaidArray` is that single
composed-device abstraction (`AGENTS.md` §27), modelled on Linux md's
per-personality dispatch: it is an enum over the six engines that forwards
both the `Block` I/O path (`geometry`/`device_class`/`read`/`write`/`flush`/
the class-aware and discard/health surface) and the level-agnostic observation,
maintenance, and reconfiguration operations, so neither the autoloaded serve
process nor the ARXFS-native composition re-derives the level → engine mapping
(`AGENTS.md` §2.2).

The wrapper is a thin, allocation-free dispatch layer: it borrows the concrete
engine (which in turn borrows its caller-owned member slice, so there is no
fixed member ceiling, `AGENTS.md` §24.1) and adds **no policy of its own** —
every arm forwards to the engine whose behaviour is proven in the sections
above.

- **Observation** — `level`, `health` (mapped onto the shared `ArrayHealth`
  vocabulary), `member_count`, `array_geometry`, `member_state`,
  `needs_resync`, `scrubbing`, `scrub_cursor`.
- **Self-maintenance** — `begin_scrub` / `scrub_step` / `resync_step`. The
  unified maintenance methods take one **scratch** buffer that both sizes the
  bounded chunk (its length in whole array blocks, so a 100 TB+ array never
  scrubs or rebuilds in one sweep, `AGENTS.md` §26.6, §2.23) and serves as the
  staging buffer for the mirror; the parity levels use it only to size the
  budget and stage through their own assemble-time scratch, while RAID10 (like
  the mirror) stages through the scratch directly. A scratch that is empty or
  not a block-size multiple fails closed with `RaidError::BadScratch`.
- **Reconfiguration** — `readd_member` / `remove_member` / `add_member` /
  `replace_member`, the hot-swap workflow, each mapping the engine's
  composition-policy outcome onto the shared `RaidError`.

### The stripe has no redundancy — the dispatch is honest about it

A RAID0 stripe has nothing to scrub from, rebuild from, or hot-swap, so every
redundancy-only operation on the stripe arm **fails closed** with
`RaidError::NotRedundant` rather than pretending to succeed — the same honesty
the stripe engine shows by reporting only `Optimal` / `Failed`. The level check
wins over scratch validation, so the caller always learns the informative
reason. Its `Block` I/O path, `level`, `health`, `member_count`,
`array_geometry`, and `member_state` (a stripe member maps to `InSync` when
live and `Faulted` when dropped) forward normally.

## Maintenance scheduling (`ArrayMaintenance`)

Exposing a self-healing surface is not the same as driving it. `RaidArray`
offers `readd_member`, `resync_step`, and `begin_scrub`/`scrub_step`, but an
array only heals itself if something decides, turn by turn, which of those to
do next — and, just as importantly, when to do none of them so the foreground
workload keeps the array (`AGENTS.md` §26.1, §26.2, §2.16). Every consumer that
owns a composed array — the autoloaded serve process and the ARXFS-native
multi-device composition alike — needs exactly that decision, and getting it
wrong is a data-integrity or availability fault, not a cosmetic one:

- A rebuild that is never started leaves the array degraded until the *next*
  fault loses data (`AGENTS.md` §26.5).
- A rebuild that never yields starves the workload the array exists to serve.
- Re-probing a faulted member in a tight loop is the busy-wait the charter
  forbids (`AGENTS.md` §2.23); never re-probing it means a disk that came back
  stays out of the array (`AGENTS.md` §18.4).
- Scrubbing an array that is mid-rebuild spends the bandwidth the rebuild needs
  to restore redundancy.

`ArrayMaintenance` is that one decision, defined once so the consumers cannot
hand-roll it differently (`AGENTS.md` §2.2, §27). It is pure and **event-timed**:
it holds no clock, arms no timer, and never spins. The caller supplies the
monotonic reading it took on every entry point, and when there is nothing to do
`wait_deadline_ns` gives the absolute one-shot deadline the serve loop parks on
— the same idiom the per-device health machine and the fault domain use
(`blkio::BlkHealth::grace_deadline_ns`). It is allocation-free: the per-member
re-add backoff records live in a caller-owned slice, exactly as the engines'
members do, so a wide array imposes no fixed ceiling (`AGENTS.md` §24.1).

The serve loop's contract per turn is: `next_action` to decide, perform the
action against the array, `note_step` to hand back what happened (which is what
paces the next chunk and escalates a refused re-add), and on `Idle` park until
the soonest of the array's own I/O and `wait_deadline_ns`. Foreground traffic is
reported through `note_foreground`, and a member's demonstrated return — the
recovery signal its leaf health machine or its fault domain publishes (IO3/IO4)
— through `note_member_returned`.

### Priority — restore redundancy, then verify it

1. **Re-admit a faulted member** whose backoff has elapsed. An array short a
   copy is one fault from data loss, so getting the copy back outranks
   everything else.
2. **Advance a rebuild** of a member that is already resyncing.
3. **Advance or start a proactive scrub**, and only on a fully `Optimal` array.
   While a copy is missing or rebuilding, the bandwidth belongs to restoring
   redundancy, and a scrub that can detect but not repair spends I/O to no
   benefit. An array that degrades mid-pass therefore *pauses* its scrub where
   the cursor stands and resumes it once full redundancy is back, rather than
   abandoning the work already done or pressing on without a copy to repair
   from.

### Pacing — a duty share, not a fixed rate

An idle array runs maintenance flat out. While the array is also serving
foreground I/O, a chunk that took `d` holds the next one off for
`d × (100 − duty) / duty`, so maintenance keeps to its share of the array
whatever chunk size the caller's scratch buffer implies — unlike a fixed
bytes-per-second limit, which has to be retuned for every device. A share
outside `1..=100` is clamped, so a mis-set policy can neither stall maintenance
completely nor divide by zero. A chunk the members failed backs off by the
class's recovery grace window rather than hammering hardware that is already
unwell.

### Cadences come from the array's discovered class

`MaintenancePolicy::for_class` derives the defaults from the class the members'
fold declares through `Block::device_class` (see [Device class](#device-class-device_class)),
never a frozen scalar (`AGENTS.md` §24.2). The two genuinely hardware-dependent
quantities come from that class's own `IoBudget` rather than a second table that
could drift from it (`AGENTS.md` §2.2):

- The **first re-add delay** is the class's recovery grace window (`grace_ns`).
  Re-probing sooner asks a device that is still inside the window its own driver
  gives it to come back. The delay doubles on each refusal up to 32× it, so a
  dead disk is not re-probed at the cadence of a merely slow one, yet a disk
  that returns after a long absence still rejoins within a bounded wait. A
  recovery signal collapses an escalated wait back to the base delay after the
  last attempt and no further, so neither a flapping member nor a repeating
  signal can turn the hook into a re-probe storm.
- The **busy duty share** reflects how destructive maintenance is to foreground
  latency on that class: a rotational disk pays for every extra seek and a
  removable unit has a shallow queue that saturates as easily, so both keep a
  small share; a solid-state device absorbs a parallel background stream with
  far less interference, and a paravirtual device sits between them.

The scrub period and the busy window are properties of the accepted risk and of
the workload rather than of the hardware, so they are one default for every
class and are overridable per array through the policy's public fields. The
period is measured end-of-pass to start-of-pass. `ArrayMaintenance::new` takes
how long ago the last pass completed, as the caller knows it from the array's
persisted maintenance record; a caller with **no** record passes `u64::MAX`,
which makes the first pass due immediately — an array whose verification history
is unknown is verified rather than assumed clean (`AGENTS.md` §5.4, §26.5), and
the duty pacing bounds what that costs.

An array handed over **mid-pass** — one whose cursor was restored from its
maintenance record (below) — is adopted as such, so finishing that pass re-arms
the period like any other. Were the resumed pass treated as none of the
scheduler's doing, its completion would go unnoticed and the already-overdue
period would start it again at once, verifying the array back-to-back forever.

### What it deliberately does not do

- It never installs or removes a device. `add_member` / `remove_member` are the
  operator/hotplug hot-swap workflow; an `Absent` slot has no device to
  re-probe, so the scheduler leaves it alone rather than inventing a spare.
- It drives nothing on a `Failed` array. With no in-sync member there is nothing
  to rebuild a returning copy from, and admitting one as current would serve
  data the array cannot vouch for (`AGENTS.md` §5.4, §26.5). Bringing a failed
  array back is a re-resolution of its members' superblocks against their
  generation counters — an assembly decision, not a maintenance one.
- It drives nothing on a non-redundant RAID0 stripe. Whether a level has
  redundancy at all is `RaidLevel::is_redundant`, the single definition the
  composed-device dispatch also refuses its redundancy-only operations with, so
  the two cannot disagree about which arrays can heal themselves.

## Durable maintenance progress (`MaintenanceRecord`, `ArrayProgress`)

A scrub and a rebuild both advance a cursor one bounded chunk at a time, so on
a 100 TB+ array a full pass runs for **hours or days** — longer than the
interval between reboots on a real machine. If the cursor lived only in memory,
every restart would silently discard the work and begin again: an array
rebooted often enough would never finish a rebuild, and might never be verified
at all. That is a latent, unbounded data-integrity hole exactly where
redundancy is supposed to protect the most data (`AGENTS.md` §26.5, §26.6), so
the position is durable.

`ArrayProgress` is the resumable position — the scrub cursor and the rebuild
cursor, each `None` when that pass is not running. It is the *same* value the
engines report and accept and the on-disk record carries, so the in-memory and
persisted notions of "how far have we got" are one definition (`AGENTS.md`
§2.2). Every level answers `RaidArray::progress()` (a non-redundant stripe has
nothing in progress and reports the idle position), and every redundant level
accepts `RaidArray::restore_progress()` once after assembly, before the first
maintenance step.

Two rules make the report and the restore safe:

- **The reported rebuild cursor is the *least advanced* member's.** Several
  members can rebuild at once at different cursors, and one record carries a
  single position. Resuming from the least advanced re-copies blocks a
  further-ahead member already holds — harmless, because a rebuild write is
  idempotent — whereas resuming from the furthest ahead would leave another
  member's outstanding blocks never copied while the array counted it fully
  rebuilt.
- **A cursor outside the array is refused, never clamped**
  (`RaidError::CursorOutOfRange`). Adopted as a rebuild position it would
  declare a member fully copied without its tail ever having been written,
  leaving stale data trusted as a current read source (`AGENTS.md` §5.4,
  §26.5). A restored cursor is also planted only on members that are actually
  rebuilding, so it can never un-sync a current copy.

`MaintenanceRecord` (in `lib/raidmeta`, beside the superblock so the
composition driver and the discovery probe share one definition) is the on-disk
form each member carries in the block after its superblock
(`MAINTENANCE_BLOCK`). Keeping it a *separate* record in a *separate* block is
deliberate: the superblock changes only when the array's shape does, while
progress is checkpointed continuously as the array works, and a torn write of a
routine checkpoint must never be able to damage the metadata assembly depends
on. A member's share of the array's data therefore begins at
`RESERVED_METADATA_BLOCKS`, the single definition of that offset.

Beside the cursors the record carries the `Time64` instant the last **complete**
verification pass finished — the value `ArrayMaintenance::new` is seeded with.
That stamp deliberately survives a membership change: verifying the array is a
property of the data, not of the member set.

Every way of losing or doubting the record degrades toward *more* verification,
never less:

| Situation | Outcome |
| --- | --- |
| Absent, blank, torn, or corrupt (CRC-32C) | No position; passes start from the beginning |
| Written by another array (UUID mismatch) | Ignored entirely |
| From an earlier array generation | Cursors ignored; the completion stamp still honoured |
| Completion stamp ahead of the wall clock | Read as "unknown"; a pass is due at once |
| Cursor outside the array | Refused; the array stays at its fresh-start position |

An earlier generation invalidates the cursors because a member has joined or
left since, so a resumed cursor could skip data the new member never received.
An implausible future stamp — an unset or stepped clock, or a forged record —
must not be able to suppress verification indefinitely, so it reads as unknown
rather than as "recently clean". Because the checksum, the identity binding and
the canonical-encoding checks are all enforced on decode, a hostile or failing
disk cannot use this record to make an array *skip* work: the worst a bad
record achieves is being discarded. The decoder is fuzzed for panic-freedom,
including against an adversary that reseals a corrupted record with a valid
checksum (`AGENTS.md` §19.6).
