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
data back, forcing the device to reallocate the sector — the auto-scrub a
mirror exists to provide. Only a *whole-device* fault
(`DeviceOffline`/`DeviceFault`, or a member returning a request-level error for
a request the array already validated) drops the copy from the array. A read
with no surviving copy fails closed and never fabricates data (`AGENTS.md`
§5.4).

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

## On-disk metadata and reassembly (`ArraySuperblock`, `ArrayIdentity`)

An array is **discovered, not configured**: there is no hand-maintained list
of which devices form an array (`AGENTS.md` §18, §16.5). Each member carries a
fixed-size, little-endian `ArraySuperblock` naming the array (a 128-bit
`ArrayUuid`), the RAID level, the total member count, this member's slot, the
array geometry, a monotonic **generation** counter, and a `Time64` last-write
stamp. The record is sealed with a trailing CRC-32C (`lib/crc32c`, the one
first-party checksum) — a media/transport integrity check, not a security
control: an array's authenticity rests on the signed driver bundle and the
members' own capability-gated block endpoints, not on this value.

`ArraySuperblock::decode` **fails closed** on any malformed on-disk byte
(`AGENTS.md` §5.4, §26.5) — a bad magic, an unknown version, a checksum
mismatch, an unknown RAID level, a zero member count, a slot outside the
array, a degenerate geometry, or a non-canonical timestamp is a typed
`SuperblockError`, never a silently-trusted record. The decoder is total and
`forbid(unsafe_code)`, and a fuzz harness (`tests/fuzz_superblock.rs`,
`AGENTS.md` §19.6) proves it never panics on arbitrary input and that every
accepted record round-trips.

Reassembly resolves the array from a set of discovered `Candidate` members:

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

The on-disk format is unfrozen pre-release (`AGENTS.md` §2.13): it is changed
in place, never versioned alongside an old one.

### States

A member is `InSync` (a full copy: read source and write target), `Faulted`
(dropped after a whole-device fault or a failed write; neither serves nor
receives I/O until re-added), or `Resyncing` (being rebuilt; receives
synced-region writes, not yet a read source). A faulted copy is
sticky-but-recoverable: it stays out of the array until it demonstrably returns
through `readd_member`/`replace_member`, so a flapping disk cannot masquerade
as a healthy copy, yet a genuine return always rejoins without a reboot
(`AGENTS.md` §18.4).
