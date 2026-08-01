# `tairix-drv-storage-raid`

TAIRiX **RAID composition** driver: fault-aware virtual block devices that
compose child block endpoints through the public block seam
(`plans/FIX-IO.md` IO6; `docs/src/drivers/raid.md`).

Stability tier: **experimental**.

## What it is

A RAID volume is itself a
[`tairix_abi::driver::block::Block`](../../../lib/abi): it composes several
child `Block` copies and presents one logical device to the filesystem layer,
so a composed array nests naturally over the same seam every leaf device uses
(`AGENTS.md` §2.2 one seam, §27 complete abstraction). It **consumes** the
block-layer health vocabulary (`tairix_abi::blkio`); it does not re-invent it.

Six compositions are provided as siblings over that one seam (`AGENTS.md`
§2.2 parallel implementations): the redundant **RAID1 mirror**
(`MirrorArray`), the capacity-aggregating **RAID0 stripe** (`StripeArray`), the
single-fault **RAID5 distributed parity** (`ParityArray`), the two-fault
**RAID6 double distributed parity** (`DualParityArray`, P + Q Reed-Solomon
syndromes over the first-party `gf256` GF(2^8) field), the three-fault
**RAID-TP triple distributed parity** (`TripleParityArray`, P + Q + R over the
same field), and the **RAID10 stripe of mirrors** (`Raid10Array`). They share
one `MemberState`/`MemberRole`/`ArrayHealth` vocabulary and fault
classification. A serving process drives whichever level composes an array
through the one `RaidArray` dispatch, and schedules its self-healing through
the one `ArrayMaintenance` policy (below).

The RAID1 mirror (`MirrorArray`):

- **Reads** are served from any in-sync copy; a per-block `MediumError` is
  recovered from a good copy and the bad copy is **repaired** in place
  (opportunistic read-repair), while only a whole-device fault drops a copy.
- **Scrub** (`begin_scrub`/`scrub_step`) is a bounded, interruptible pass that
  proactively reads *every* in-sync copy of *every* block and repairs a latent
  media error the read path would never consult — the auto-scrub a mirror
  exists to provide (`AGENTS.md` §26.5), chunked so a 100 TB+ array never
  scrubs in one sweep (`AGENTS.md` §26.6).
- **Writes** fan out to every copy; a copy that fails a write is dropped and
  the write still succeeds as long as one copy accepted it.
- A faulted copy **degrades the array, never the system** — the survivors keep
  serving and the array reports `Degraded`.
- A **missing member slot** (`MemberState::Absent`) is first-class, like a
  Linux md "removed" slot: the array is assembled to its full defined width
  (one `MirrorMember::absent()` per missing copy), counts the empty slot toward
  its member count, and reports `Degraded` for the reduced redundancy rather
  than masquerading as a smaller, optimal array. A failed disk is pulled with
  `remove_member` (vacating its slot and returning the device) and a fresh
  spare inserted with `add_member`, which rebuilds it from a surviving copy —
  the full remove-failed / add-spare replacement workflow, without a reboot
  (`AGENTS.md` §18.4).
- A returning copy is rebuilt by a **bounded, interruptible resync**
  (`resync_step`), so a 100 TB+ rebuild never blocks the system or busy-spins
  (`AGENTS.md` §26.6). Array health maps onto the shared
  `MountAvailability` vocabulary.

At the boundary of what it can vouch for (no surviving copy for a read, no copy
accepting a write, no copy committing a flush) the array **fails closed**
(`AGENTS.md` §5.4): the *operation* fails, the *system* keeps running.

## Maintenance scheduling (`ArrayMaintenance`)

Offering a self-healing surface is not the same as driving it. `ArrayMaintenance`
is the one policy that decides, turn by turn, whether an array should re-admit a
faulted member, advance a rebuild, run a proactive scrub, or do none of those so
the foreground workload keeps the array (`AGENTS.md` §2.2, §27, §26.1). It is
pure, allocation-free, and **event-timed** — it holds no clock and never spins:
the caller supplies its monotonic reading, and `wait_deadline_ns` gives the
one-shot deadline the serve loop parks on (`AGENTS.md` §2.23).

Restoring redundancy outranks verifying it (re-add, then rebuild, then scrub),
a scrub runs only on a fully `Optimal` array and pauses at its cursor while
redundancy is reduced, and maintenance keeps to a duty share of a busy array.
A faulted member is re-probed on a bounded, doubling cadence whose base is that
device class's own recovery grace window, so a dead disk is not hammered and a
returning one always rejoins without a reboot (`AGENTS.md` §18.4). The
scheduler never installs or removes a device, and drives nothing on a `Failed`
array — recovering one is a re-resolution of its members' superblocks, not a
maintenance decision.

## Crate shape

This crate is the host-testable composition **engine** — one module per level
(`src/mirror.rs`, `src/stripe.rs`, `src/parity.rs`, `src/dualparity.rs`,
`src/triple.rs`, `src/raid10.rs`, over the shared `src/gf256.rs` field) — plus
the level-agnostic layers above it: the `src/array.rs` composed-device
dispatch, the `src/assemble.rs` reassembly→member bridge, the `src/health.rs`
and device-class folds, and the `src/maintenance.rs` scheduler. Each is proven
host-side over a fault-injecting `Block` double (its `*/tests.rs`). It is
`no_std`, `forbid(unsafe_code)`, and allocation-free: every array borrows a
caller-owned member slice (and, for the parity levels, a scratch buffer; and,
for the scheduler, a per-member retry slice), so it imposes no fixed member
ceiling (`AGENTS.md` §24.1) and holds only a borrow. It depends only on
`lib/abi` and the shared on-disk metadata crate `lib/raidmeta`, so the layered
dependency direction holds (a member is reached through the `Block` trait the
serve process is handed, never a sibling driver crate, `AGENTS.md` §17.4).

The autoloaded serve process that assembles members from discovered array
metadata and turns this scheduler's decisions into real transfers is designed
and staged in `plans/FIX-IO.md` §2.6 (IO6a–IO6f).
