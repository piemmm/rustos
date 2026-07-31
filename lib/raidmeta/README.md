# tairix-raidmeta

Stability tier: **experimental**

The one first-party definition of TAIRiX's **RAID on-disk metadata**: the
array-member *superblock* every member of an array carries, and the fail-closed
logic that reassembles an array from a set of discovered members
(`plans/FIX-IO.md` IO6).

It is a pure, `no_std`, `forbid(unsafe_code)`, allocation-free library shared by
two independent consumers so they can never disagree about what a RAID member
is (`AGENTS.md` §2.2) without a `drivers/*`->`drivers/*` edge (§17.4):

- the **RAID composition driver** (`drivers/storage/raid`), which assembles the
  mirror / stripe / parity / double-parity / triple-parity engines from decoded
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

## On-disk placement

A member carries its superblock in the leading `WIRE_LEN` bytes of its first
block (member block 0), so a probe reads block 0 and validates the record
without knowing the array's data layout. How the array's logical data is placed
relative to that reserved metadata is established by the assembling serve
process when it is built (`plans/FIX-IO.md` IO6 remaining).

See `docs/src/drivers/raid.md`.
