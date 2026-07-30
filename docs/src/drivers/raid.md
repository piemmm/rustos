# RAID composition

A RAID volume is a **virtual block device that composes child block
endpoints** through the same fault-aware block seam every leaf device uses
(`plans/FIX-IO.md` IO6; `AGENTS.md` §2.2, §27). It is itself a
[`tairix_abi::driver::block::Block`](./block.md), so a composed array presents
one logical device to the filesystem layer and multi-layered sets nest
naturally over the recursive seam. RAID **consumes** the block-layer health
vocabulary (`blkio::BlkStatus`, `DriverError`); it never re-invents it.

The composition engine lives in `drivers/storage/raid` as a host-testable
library. The autoloaded serve process that assembles members from discovered
array metadata and drives resync off the members' recovery signals rides with
the multi-device volume-assembly work (`plans/FIX-IO.md` IO6 remaining); the
engine is proven host-side first, as the other FIX-IO primitives were.

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

### States

A member is `InSync` (a full copy: read source and write target), `Faulted`
(dropped after a whole-device fault or a failed write; neither serves nor
receives I/O until re-added), or `Resyncing` (being rebuilt; receives
synced-region writes, not yet a read source). A faulted copy is
sticky-but-recoverable: it stays out of the array until it demonstrably returns
through `readd_member`/`replace_member`, so a flapping disk cannot masquerade
as a healthy copy, yet a genuine return always rejoins without a reboot
(`AGENTS.md` §18.4).
