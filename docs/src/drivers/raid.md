# RAID composition

A RAID volume is a **virtual block device that composes child block
endpoints** through the same fault-aware block seam every leaf device uses
(`plans/FIX-IO.md` IO6; `AGENTS.md` §2.2, §27). It is itself a
[`tairix_abi::driver::block::Block`](./block.md), so a composed array presents
one logical device to the filesystem layer and multi-layered sets nest
naturally over the recursive seam. RAID **consumes** the block-layer health
vocabulary (`blkio::BlkStatus`, `DriverError`); it never re-invents it.

The composition engine and its on-disk metadata layer live in
`drivers/storage/raid` as a host-testable library. The autoloaded serve
process that reads each discovered device's superblock, assembles the members,
drives resync off the members' recovery signals, and publishes the composed
device as its own block-service node rides with the multi-device
volume-assembly work (`plans/FIX-IO.md` IO6 remaining); the engine and its
metadata are proven host-side first, as the other FIX-IO primitives were.

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

## On-disk metadata and reassembly (`ArraySuperblock`, `ArrayIdentity`)

An array is **discovered, not configured**: there is no hand-maintained list
of which devices form an array (`AGENTS.md` §18, §16.5). Each member carries a
fixed-size, little-endian `ArraySuperblock` naming the array (a 128-bit
`ArrayUuid`), the RAID level, the total member count, this member's slot, the
array geometry, a monotonic **generation** counter, a `Time64` last-write
stamp, and the stripe unit (`chunk_blocks`) for a striped level. The level and
the stripe unit must agree: a striped level (RAID0) records a non-zero
`chunk_blocks`, a full-copy level (the mirror) records zero, and a record whose
level and stripe unit contradict is refused (`SuperblockError::BadStripeChunk`)
so a corrupt or foreign record is never mistaken for a valid array. The record
is sealed with a trailing CRC-32C (`lib/crc32c`, the one
first-party checksum) — a media/transport integrity check, not a security
control: an array's authenticity rests on the signed driver bundle and the
members' own capability-gated block endpoints, not on this value.

`ArraySuperblock::decode` **fails closed** on any malformed on-disk byte
(`AGENTS.md` §5.4, §26.5) — a bad magic, an unknown version, a checksum
mismatch, an unknown RAID level, a zero member count, a slot outside the
array, a degenerate geometry, or a non-canonical timestamp is a typed
`SuperblockError`, never a silently-trusted record. (`chunk_blocks` is part of
the array shape `resolve`/`verdict_of` compare, so a member disagreeing on the
stripe unit is refused like any other shape mismatch.) The decoder is total and
`forbid(unsafe_code)`, and a fuzz harness (`tests/fuzz_superblock.rs`,
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
