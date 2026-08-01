# tairix-raidmeta

Stability tier: **experimental**

The one first-party definition of TAIRiX's **RAID on-disk metadata**: the
array-member *superblock* every member of an array carries, the durable
*maintenance record* that carries an array's scrub/rebuild progress across a
restart, and the fail-closed logic that reassembles an array from a set of
discovered members (`plans/FIX-IO.md` IO6).

It is a pure, `no_std`, `forbid(unsafe_code)`, allocation-free library shared by
two independent consumers so they can never disagree about what a RAID member
is (`AGENTS.md` §2.2) without a `drivers/*`->`drivers/*` edge (§17.4):

- the **RAID composition engines** (`lib/raid`), which assemble the
  mirror / stripe / parity / double-parity / triple-parity levels from decoded
  members, and
- the **storage-discovery probe** (`lib/fsprobe`, used by `drivers/storage/volmgr`),
  which recognises a member so a bare, un-assembled array member is never
  mounted as a standalone filesystem (the stale-read / divergent-copy hazard,
  §26.5).

## What it provides

- `ArraySuperblock` — the fixed-size, little-endian on-disk record (identity,
  `RaidLevel`, member count/slot, geometry, monotonic generation counter, and a
  `Time64` last-write stamp, §21), sealed with a trailing CRC-32C
  (`lib/crc32c`, an integrity check — not a security control). `encode` /
  `decode` fail closed on any malformed byte (`AGENTS.md` §5.4) and the decoder
  is fuzzed for panic-freedom (`tests/fuzz_superblock.rs`, §19.6).
- `RaidLevel` — the composition a superblock describes, with the shared
  `min_members` / `max_members` / `data_members` / `logical_block_count`
  structural rules the composition engines also read.
- `MAX_PARITY_DATA_MEMBERS` — the single structural ceiling on the GF(2^8)
  parity levels' (RAID6 / RAID-TP) data-member count dictated by their
  Reed-Solomon syndromes; the parity engines' fields derive from it.
- `ArrayIdentity` / `Candidate` / `distinct_arrays` — the pure reassembly: the
  freshest member (highest generation) fixes the authoritative array shape, and
  each member is placed as in-sync, a **stale** rebuild target, missing, or
  refused, from one decision so the slot table and the per-member verdict cannot
  diverge.
- `MaintenanceRecord` / `ArrayProgress` — the durable position of an array's
  self-maintenance: the scrub and rebuild cursors, and the `Time64` instant the
  last *complete* verification pass finished. A pass over a 100 TB+ array runs
  for hours or days, so without it every restart would silently discard the
  work and begin again — an array rebooted often enough would never finish a
  rebuild, or never be verified at all (§26.5, §26.6). Sealed with its own
  CRC-32C, bound to the array's UUID *and* generation, canonically encoded, and
  fuzzed (`tests/fuzz_maintenance.rs`). Every way of losing it — absent, torn,
  foreign, or from a superseded membership — degrades toward *more*
  verification, never less, so a hostile or failing disk cannot use it to make
  an array skip work.

## On-disk placement

A member carries its superblock in the leading bytes of its first block
(`SUPERBLOCK_BLOCK`), so a probe reads block 0 and validates the record without
knowing the array's data layout, and its maintenance record in the next block
(`MAINTENANCE_BLOCK`). They are separate blocks so a routine progress
checkpoint can never tear the metadata assembly depends on, and so the two can
be written at completely different rates. A member's share of the array's
*data* begins at `RESERVED_METADATA_BLOCKS`, the single definition of that
offset every consumer derives (§2.2).

See `docs/src/lib/raid.md`.
