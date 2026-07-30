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

The first composition is the **RAID1 mirror** (`MirrorArray`):

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

## Crate shape

This crate is the host-testable composition **engine** (`src/mirror.rs`),
proven host-side over a fault-injecting `Block` double (`src/mirror/tests.rs`).
It is `no_std`, `forbid(unsafe_code)`, and allocation-free: `MirrorArray`
borrows a caller-owned member slice, so it imposes no fixed member ceiling
(`AGENTS.md` §24.1) and holds only a borrow. It depends only on `lib/abi`, so
the layered dependency direction holds (a member is reached through the
`Block` trait the serve process is handed, never a sibling driver crate,
`AGENTS.md` §17.4).

The autoloaded serve process that assembles members from discovered array
metadata and drives resync off the members' recovery signals rides with the
multi-device volume-assembly work (`plans/FIX-IO.md` IO6 remaining).
