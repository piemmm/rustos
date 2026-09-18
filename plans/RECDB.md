# RECDB.md — `lib/recdb`: the durable, crash-safe, indexed record store

Binding under `AGENTS.md`. TAIRiX has no database engine. Structured state is
persisted today either as a whole-file rewrite of a text document
(`/System/Security/Users` through `lib/users`) or as an opaque blob a caller
must parse in full (`lib/appdata`'s descriptor-backed `Blobs/`). Both are
correct for what they were built for and neither survives a consumer with many
records, partial updates, secondary lookups, or a requirement to remain
consistent across a power cut.

This plan owns the engine that does: a first-party, `no_std`, crash-safe B+tree
record store with write-ahead logging, checksummed pages, snapshot reads,
bounded memory, and optional encryption at rest.

Read first (§15.18): `AGENTS.md` §2.12 (roll your own), §4 (allocator, OOM,
zero-on-free), §21 (64-bit-native time and storage), §24 (capacities scale,
bounds stay fixed), §26.3/§26.5/§26.6/§26.7 (memory pressure, a failing disk,
very large stores, and the combined floor), §27 (complete, not minimal);
`plans/ARXFS-WRITEBACK.md` (the commit barrier this builds on — `fs_sync`
already means "make it durable"), `plans/APPDATA.md` (the blob descriptors a
store file is reached through), `plans/FILELOCK.md` (the single-writer lock),
`plans/SMARTRAM.md` (`lib/reclaim`, which governs the page cache).

## Ledger

| # | Item | Status |
|---|---|---|
| RD0 | This plan, the jump-sheet row, the §3 map entry, the `PLAN.md` section | done |
| RD1 | The page layer: page format, checksums, the pager, the free-space map, and the `lib/reclaim`-governed page cache | planned |
| RD2 | The write-ahead log: record format, append, the commit barrier over `fs_sync`, checkpointing, and log reclamation | planned |
| RD3 | Recovery: redo from the last checkpoint, torn-page detection and repair, and the fail-closed refusal of an unrecoverable store | planned |
| RD4 | The B+tree: insert, lookup, range scan, delete, split/merge, and the bulk load | planned |
| RD5 | Transactions: the single-writer/multi-reader snapshot model, atomic multi-table commit, and rollback | planned |
| RD6 | Tables, typed records, and secondary indexes: the versioned encoding, the schema descriptor, and index maintenance inside the writing transaction | planned |
| RD7 | Encryption at rest: per-page AEAD with a nonce that cannot repeat, and the key handling that never stores a key beside its data | planned |
| RD8 | The verification battery: the crash-injection harness, the fuzz targets, the property model, and the `loom` model for the reader/writer handoff | planned |

Items are built in ledger order; each is complete — tests, docs, green gate —
before the next begins. RD8's harness is written *with* RD1–RD7 rather than
after them: a store whose recovery is untested has no property worth naming.

## 0. Binding decisions

1. **First-party, because the alternative is not available.** SQLite is C,
   which §1 and §15.11 forbid authoring, and an embedded Rust store would be a
   large external dependency in the trusted computing base for something a
   filesystem-adjacent OS ought to own (§2.12). The crypto carve-out does not
   extend here: a B+tree and a WAL are well-understood structures, not
   primitives whose hand-rolling is reckless.
2. **One writer, many readers — and that is a design, not a limitation.** A
   store has a single writing transaction at a time, held by one process (the
   file lock, `plans/FILELOCK.md`); readers run concurrently on snapshots.
   This removes write-write conflicts, lock escalation, and deadlock **by
   construction**: there is no lock cycle to detect because there is no second
   writer. A consumer needing concurrent writers decomposes into more stores or
   serialises through one owner — which is exactly what
   `plans/WINTERSUN.md`'s single store process does.
3. **Durability is the WAL plus the commit barrier, and nothing else.** A
   commit appends its records, issues the barrier, and only then reports
   success. `fs_sync` already carries that meaning through ARXFS's commit
   barrier. A store never claims a commit the barrier has not returned from.
4. **Corruption is detected and refused, never served.** Every page carries a
   checksum over its contents and its own page number, so a torn write, a
   misdirected write, and a stale page are all distinguishable. A page that
   fails verification is repaired from the log if the log covers it and
   otherwise refuses the read as a typed error. The store never returns bytes
   it cannot vouch for (§26.5, §5.4).
5. **Memory is the working set, never the store.** The page cache is a
   `lib/reclaim` cache whose budget derives from discovered RAM and which
   shrinks through the pressure bands. A multi-terabyte store opens and serves
   on a small machine; a resident structure proportional to store size is the
   §24.1/§26.6 defect this forecloses.
6. **Everything is 64-bit.** Page numbers, byte offsets, record counts, log
   sequence numbers, and every timestamp (`Time64`). Pointer width is not
   storage width, on any target including `wasm32` (§21, §26.6).
7. **No panic on any path.** Every operation returns a `Result`; allocation
   failure, exhaustion, corruption, and a failing device are values (§2.9, §4).
8. **Capacities scale, bounds stay fixed.** The cache, the free-space map, the
   log, and the tree grow with the machine and the data (§24.1). The
   *validation* bounds — maximum key length, maximum record length, maximum
   tree depth, maximum log record size — are fixed security bounds and do not
   move to accommodate a caller (§24.4).
9. **The crate holds no policy about what it stores.** It has no notion of a
   user, a game, or a capability; a consumer gates access before it opens a
   store. Keeping authorisation out of the engine is what lets it be audited as
   a data structure.

## 0a. The obvious challenge: why not an append-only log and snapshots?

It is the standard persistence pattern for a game server, it is far simpler
than this, and a reviewer should ask. The answer is that it is a good fit for
*one* of this store's consumers and a poor fit for the others, and the
difference is queries.

An append-only log with periodic snapshots gives durability and crash recovery
cheaply, and for a realm's live world state it would be sufficient. What it
does not give is **a lookup that is not a scan**: "the account named X", "this
character's inventory", "log entries matching this subject in this interval",
"the blob index for this bundle id". Each of those is a keyed or ranged query
whose cost under a log-and-snapshot scheme is the whole dataset, or a
hand-rolled index beside the log — which is this engine, arrived at by
accident and without its tests.

There is also a cost argument that runs the other way, and it is worth being
honest about: the log-and-snapshot design would be perhaps a fifth of the work.
That is a real saving and it is declined for a stated reason — the OS consumers
(§6) need the indexes, a second persistence mechanism beside this one would be
the duplication §2.2 forbids, and a B+tree with a WAL is well-understood
engineering rather than research.

What the log pattern *does* win is adopted rather than dismissed: the WAL (§2)
**is** an append-only log, and the checkpoint is the snapshot. The difference is
that the tree is maintained as the authoritative index instead of being rebuilt
by replay, so a query is a descent rather than a scan.

## 0b. Throughput, and what happens when the disk cannot keep up

A single writer serialises every commit in the system, so its throughput is a
realm's throughput and the number must be known rather than hoped for.

- **The budget is stated and measured** (§8): commits per second at a given
  record size, on both a rotational and a solid-state device, since a barrier
  costs radically different amounts on each. A consumer sizing its commit
  cadence against this is why `plans/WINTERSUN.md` separates its tick from its
  commit interval.
- **Back-pressure is explicit and bounded.** The submission queue has a depth;
  reaching it makes a submission block or fail as a typed error at the caller's
  choice, never grow without limit (§4, §24.3). A consumer therefore learns
  that the disk is behind, and can shed or coalesce its own work.
- **A slow device is not a failing device**, and the two are distinguished: a
  commit that is merely slow is waited on, a commit whose device reports an
  error or exceeds its deadline surfaces as a typed error and marks the store
  degraded (§26.5). Conflating them is how a system ends up retrying forever
  against a dying disk (§2.1).
- **Durability-critical and routine commits are the consumer's distinction, not
  the engine's.** The engine offers a commit that returns when durable; batching
  policy belongs to the consumer that knows which of its writes a user would
  mind losing.

## 1. RD1 — pages

A store is one file, reached through an ordinary descriptor, divided into
fixed-size pages. The page size is a store-creation property recorded in the
header, defaulting to a value chosen against the filesystem's own block size
rather than a hand-picked constant.

Every page carries a header: its page number, its kind, the log sequence number
of the change that last wrote it, and a `lib/crc32c` checksum over the page
including its own number. Checksumming the number is what turns a *misdirected*
write — a correct page written to the wrong place — from silent corruption into
a detected fault, and it costs nothing.

The store header is written in **two copies** with a generation counter, so a
crash during a header update leaves at least one valid copy; the higher valid
generation wins, and neither valid means the store refuses to open. The pattern
is ARXFS's mirrored root and is adopted deliberately rather than reinvented.

Free space is a paged bitmap hierarchy, so finding a free run is
sub-linear in store size and the map itself is not resident (§26.6). A page
that held key material or a credential is zeroed before reuse and before the
buffer leaves the cache (§4).

## 2. RD2/RD3 — the log and recovery

**The log is physical redo.** A committing transaction appends, for each page
it dirtied, the page image and its new LSN, then a commit record carrying the
transaction's LSN range and a checksum over it. Physical redo is chosen over
logical because recovery then needs no knowledge of the B+tree's invariants —
replaying a page image cannot leave a half-rebalanced tree, which is the
failure mode logical redo has to reason its way out of.

**Commit is: append → barrier → publish.** The log records are durable before
the commit record, and the commit record is durable before the transaction is
reported committed. A checkpoint writes back dirtied pages, issues the barrier,
and then advances the log's reclaim point — in that order, so a crash between
any two steps leaves a log that still covers every page the checkpoint had not
proven durable.

**Recovery** scans forward from the last checkpoint, verifies each log record's
checksum, stops at the first record that is incomplete or fails verification
(which is exactly the tail a crash truncated), and replays every complete
committed transaction. A page whose checksum fails is overwritten by its
logged image if the log covers it. If recovery cannot reach a consistent state
— both headers invalid, a checkpoint's pages unrecoverable, a log gap — the
store **fails to open** with a typed error naming what was wrong. It does not
open partially, does not discard records to make progress, and does not guess
(§5.4). Recovery is idempotent: recovering an already-recovered store is a
no-op, so a crash during recovery is survivable.

## 3. RD4/RD5 — the tree and transactions

A B+tree per table: keys ordered by a documented total order, values inline
below a threshold and in overflow pages above it, leaves linked for range
scans. Insert, lookup, range scan (forward and backward), delete, split, merge,
and a **bulk load** that builds a packed tree bottom-up, because loading a
realm's records one insert at a time is the premature pessimisation §2.16
forbids. Per §27 the operation set is the one the abstraction implies, complete
from the start — a tree that can only append is not a tree.

**Transactions are snapshots.** A reader takes the current committed LSN and
sees exactly the state at that point, unaffected by a concurrent writer; the
writer's dirty pages are private until commit. Old page versions a live reader
still needs are retained and reclaimed when the last reader that could see them
finishes — so a long scan neither blocks a writer nor observes a torn view. A
writer may commit changes across several tables in one transaction, which is
what lets a consumer keep two structures consistent (a record and its index; a
character and its inventory) without a second mechanism. Rollback discards the
private pages and touches nothing durable.

## 4. RD6 — tables, records, and indexes

A **table** is a named B+tree with a schema descriptor: its key encoding, its
record encoding, its version, and its declared secondary indexes. Record
encoding is versioned, fixed-shape, little-endian, and its decoder is total and
bounded — a store file is untrusted input, because a consumer may open one that
another program or an earlier version wrote (§19.5, §24.4). A record the
decoder cannot fully understand yields no record, never a guess, following
`lib/users`' fail-closed parsing discipline.

A **secondary index** is another tree mapping an extracted key to the primary
key. Index maintenance happens **inside the writing transaction**, so an index
can never disagree with its table: there is no window in which a commit has
landed and its index has not. A consumer declaring an index declares the
extractor; the engine does the rest.

**Schema evolution before the first release is a recreate, not a migration.**
§2.13 forbids a migration path, a fallback for old data, and a `v1`-beside-`v2`
reader, and a store is not exempt from it: a schema version the engine does not
recognise makes the store **refuse to open**, with the mismatch named, and the
consumer recreates it. For the game that means a schema change wipes characters
pre-release, which is the correct cost of an unfrozen format and is far cheaper
than carrying reader variants nobody can test against real old data. An earlier
draft of this plan said a version bump "migrates records on a one-pass
rebuild"; that contradicted the very rule it cited, and is deleted rather than
softened.

Post-release the requirement genuinely inverts — a shipped world cannot be
wiped — so a real migration facility becomes necessary work, planned then,
under the ABI-freeze discipline that will apply. Writing it now would be the
speculative surface §2.4 forbids.

## 5. RD7 — encryption at rest

Optional per store, and when enabled it is per page, with `lib/crypto`'s
audited AEAD (ChaCha20-Poly1305).

The nonce is the one detail that must be exactly right, because AEAD nonce
reuse under one key is catastrophic: it is derived from **(page number, page
LSN)**, and since a page's LSN strictly increases every time it is written, the
pair is unique for the lifetime of the key. A rewrite of the same page at the
same LSN cannot occur, and the invariant is asserted in the write path rather
than assumed. Key rotation rewrites the store under a new key rather than
attempting to mix two.

The key is supplied by the caller at open — derived through `lib/crypto`'s KDF
from an operator secret, or handed over by the app-data service's sealed scope
(`plans/APPDATA.md`) — and is **never written into the store or beside it**.
The key material is held in a zeroising wrapper and scrubbed on drop; a
decrypted page holding it is zeroed before its buffer is reused (§4). An
authentication failure is a typed refusal, never a fallback to the ciphertext.

## 6. Consumers

`plans/WINTERSUN.md` WS7 is the first consumer and the one that drives RD1–RD7.
The crate is deliberately general because three in-tree consumers are already
poorly served by what exists, and each is **named here as follow-on work with
its own staging, not quietly assumed**:

- **The account database.** `lib/users` rewrites the whole file for any change
  and offers no index — a real scaling defect on a machine with many accounts.
  Moving it is a boot-critical path change (login, the installer, the image
  builder) and is therefore its own staged work, not a side effect of this
  plan.
- **The system log's index.** `journald` can answer "the last N entries"
  cheaply and "entries matching this subject in this interval" only by
  scanning. A secondary index is the natural fix (`plans/SYSLOG.md`).
- **The app-data blob index.** `confd` tracks per-app blobs whose count is
  unbounded by design (`plans/APPDATA.md`).

Deferring those migrations is not a §2.19 deferral of a defect: the crate is
complete in itself, and each migration is new work with its own risk and its
own tests. What would be a defect is *assuming* them — so they are named,
staged separately, and the engine is not shaped speculatively around them
(§2.4).

## 7. Refused by name

- **An external embedded database, or a C one.** §1, §2.12.
- **Multiple concurrent writers**, and the lock manager, deadlock detector, and
  conflict-resolution policy they would require (decision 2).
- **A SQL surface, a query planner, or an expression language.** Consumers know
  their access paths; a planner is a large surface and an untrusted-input
  parser for no present need (§2.3, §2.4).
- **Logical redo**, for the recovery-complexity reason in §2.
- **Opening a store that fails recovery**, in any mode, including a "best
  effort" or "repair what you can" flag. A tool that salvages readable records
  from a broken file may exist; it never pretends to be the store opening.
- **A fixed page-cache size, or any capacity as a hand-picked constant** (§24.1).
- **Turning a fixed validation bound into a growable capacity** to accommodate
  an oversize key or record (§24.4).
- **Storing an encryption key in, beside, or derivable from the store.**

## 8. Verification

- **Crash injection is the headline test.** A harness runs a workload against
  a simulated device that can fail a write at **any** byte boundary, tear a
  page, reorder writes across a missing barrier, or stop entirely; recovery
  then runs and the store must satisfy, for every injection point: every
  committed transaction is present and complete, no uncommitted transaction is
  visible, and every index agrees with its table. Exhaustive over injection
  points for a small workload, randomised over a large one.
- **Torn pages and misdirected writes** are detected: a half-written page, a
  correct page at the wrong number, and a stale page each produce their typed
  error or their log-driven repair, never silent bad data.
- **Snapshot isolation** holds: a reader through a concurrent writer's commit
  sees its whole snapshot and none of the commit; a long scan neither blocks
  the writer nor tears.
- **Tree invariants** after every operation in randomised sequences: ordering,
  balance, key/child counts, leaf links, no leaked page, free-space map exactly
  the complement of reachable pages.
- **Property model** (`cargo xtask proptest`): a model map beside the store
  under random operation sequences, asserting identical answers and identical
  range scans.
- **Fuzz targets** (§19.6): the page decoder, the log-record decoder, the
  record and schema decoders, and a whole-store-file target — a store file is
  untrusted input. Crashing inputs enter the regression corpus with a unit test.
- **`loom`** models the reader/writer handoff and the page-cache publication —
  both are `Acquire`/`Release` pairings whose correctness is an ordering claim,
  so the model is mandatory rather than optional (§19.11).
- **`miri`** enrolment for the crate from RD1, since the pager is the one place
  `unsafe` is plausible; if the final design carries none, that is stated in
  the completion report rather than left silent.
- **Encryption**: a nonce never repeats across any write sequence (asserted in
  the write path and tested by exhausting a small store); a tampered page fails
  authentication; a wrong key refuses cleanly; key material is absent from the
  file and zeroed after use.
- **Scale and the floor** (§26.6, §26.7): a store far larger than RAM opens,
  scans, and commits on a small discovered-RAM configuration with bounded
  resident memory and no panic; several such stores open at once; exhaustion
  fails closed as a typed error after growth is attempted.
- **Performance** (§2.16): point lookup, range scan, and commit throughput are
  measured with a complexity argument, so a later change that regresses them is
  a defect against a number rather than a matter of opinion.
