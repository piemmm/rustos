# COLLECTIONS — The shared container and hashing libraries

Status: **in progress** — C0 through C3 landed; the remaining containers are
still to come. The ledger below is the authoritative record of what is left.

## The ledger

Order is top to bottom, but `Depends on` is the binding constraint, not the
row order: an increment may start as soon as every increment it names is
`done`, and rows with no edge between them may proceed in parallel across
sessions. C5 and C7 depend on nothing and can be taken first if their
defects are the pressing ones.

The table **gains rows**: a defect too large for the increment that found it
is inserted as the next row to be taken, ahead of everything below it, rather
than deferred. When an increment lands, its `Status` cell changes and §7's
detail becomes its done-state summary — nothing is appended, here or there.

| # | Increment | Delivers | Depends on | Status |
|---|---|---|---|---|
| C0 | Hashing | `lib/hash`: `SipHash13` with its published test vectors, `FastHash`, `HashSeed` and the boot/spawn publication seam | — | **done** |
| C1 | Hash containers | `HashMap` / `HashSet` and their `BuildHasher` shims, the `lib/cpuops` group-scan ops table, and the `cargo xtask miri` stage | C0 | **done** |
| C2 | Sequences | `ArrayVec`, `SmallVec`, `ArrayString`, `RingBuf`, `SecretRing` | — | **done** |
| C3 | Intrusive list | `IntrusiveList` — the primitive C4 and C8 are built on | — | **done** |
| C4 | Recency | `LruMap`, O(1) touch / insert / evict | C1, C3 | **planned** |
| C5 | Intervals | `RangeMap`, `RangeSet` | — | **planned** |
| C6 | Identity | `SlotMap`, `IdAllocator`, `BitVec` | — | **planned** |
| C7 | Sparse index | `RadixTree` with tagged iteration | — | **planned** |
| C8 | Deadlines | `IndexedHeap`, `TimerWheel` | C3 | **planned** |
| C9 | Concurrent tier | `SpscRing`, `MpscQueue`, `ConcurrentMap`, with loom models | C1, C3 | **planned** |
| C10 | Closing sweep | docs, stability tiers, and the `deps-check` rule against re-rolling a container | C0–C9 | **planned** |

---

Binding under `AGENTS.md`. TAIRiX has `alloc` (`Vec`, `String`, `BTreeMap`,
`BTreeSet`, `VecDeque`, `BinaryHeap`), one 256-bit fixed bitset, and — since
C0 and C1 — `lib/hash` and the hash tier of `lib/collections`. The rest of the
containers a kernel actually runs on — a generational slot map, an interval
map, a sparse radix index, a hierarchical bitmap, an O(1) LRU, a timing
wheel, a lock-free ring — still do not exist, so each subsystem has grown its
own.

That is the defect this plan closes, and what remains of it is measurable
today:

* **Five independent LRU caches**, four of them with a byte-identical
  `evict_until` loop over a `(tick -> key)` `BTreeMap` recency index:
  `kernel/tairix-kernel/src/block_cache.rs`,
  `kernel/tairix-kernel/src/transform_cache.rs`,
  `kernel/core/src/launch_cache.rs`, `kernel/core/src/fs/fscache.rs`, and
  `drivers/filesystem/arxfs/src/dedupe.rs`, with a sixth embedded inside
  `lib/reclaim/src/cache.rs`. All are O(log n) where O(1) is standard.
* **A sixth LRU that is O(n)** on a packet data path:
  `lib/net/src/neigh.rs` resolves every transmit through
  `entries.iter().position(|e| e.ip == ip)` and evicts by
  `min_by_key(last_used)` — a linear scan of the neighbour table per
  packet, on the path §26.4 says is fed by hostile remote clients.
* **Four hand-rolled address-range maps**, each a `BTreeMap<u64, len>` with
  its own overlap arithmetic: `kernel/mem/src/anon_window.rs`,
  `kernel/mem/src/mmio.rs`, `kernel/core/src/aspace.rs` (twice — file and
  anonymous regions), and `drivers/filesystem/arxfs/src/runs.rs`.
* **Monotonic `next_id` counters** standing in for identifier allocation in
  `kernel/core/src/sharedreg.rs` and all three schedulers, with no reuse and
  no generation — so a recycled identifier aliases a dead object rather than
  being rejected.
* **A `BTreeMap` per futex bucket** (`kernel/core/src/futex.rs`), which is a
  hash table with the hashing removed.

Every one of those is the duplication the charter forbids, and several are
the §27.2 "O(n) linear scan on a load-bearing path" defect by name.

Read first: `lib/inline/src/bitset.rs` (the oldest container here),
`lib/reclaim/src/cache.rs` (the closest thing to a shared cache engine, and
the pressure model every cache-shaped container must integrate with rather
than duplicate), `lib/sync/src/` (locks, epoch, the loom harness),
`lib/cpuops/` (the CPU-feature dispatch framework the hash-table probe
kernel is selected through), and `tools/xtask/src/commands/bench.rs` (the
one measurement harness).

---

## 1. Shape: three crates

**`lib/hash` — landed, no external dependencies.** Hashing is not a
container, three container families need it, and putting it in
`lib/collections` would either force that foundational crate to depend on
`lib/crypto`'s whole graph (`sha2`, `hmac`, `chacha20poly1305`,
`ed25519-dalek`) or leave hash maps unseeded by default. It holds:

* `SipHash13` — the keyed pseudo-random function that defends a hash table
  against collision flooding, and the **default** hasher for every container
  here. Pinned by the published SipHash reference vectors, which are the
  oracle.
* `FastHash` — XXH64, a non-cryptographic hash for kernel-assigned keys and
  fingerprints, opt-in by naming it. Same justification `lib/rng/src/fast.rs`
  carries for xoshiro256++: an ordinary published algorithm, not a security
  primitive, pinned by the reference implementation's outputs.
* `HashSeed` — the per-boot / per-process key and the one-shot publication
  seam, described in §3.

Both hashers implement `core::hash::Hasher`, with integer writes
little-endian and pointer-sized values widened to 64 bits, so a value hashes
identically on every port. `BuildSipHash13` and `BuildFastHash` are the
`BuildHasher` shims a container constructs through; the first has no
`Default` and its `keyed()` constructor refuses before a key is published, so
a container cannot end up unkeyed by accident.

Its only dependency is `lib/sync`, for the one-shot publication cell — the
alternative was a second hand-rolled atomic state machine. It deliberately
does **not** depend on `lib/rng`: the seed is injected, so the crate stays
free of external dependencies and host-testable, and the boot path decides
where entropy comes from.

**`lib/inline` — the containers that allocate nothing, and the reason there
are three crates.** It depends on **nothing at all**, not even `alloc`, and
that is its entire purpose: a container crate that links `alloc` forces a
`#[global_allocator]` requirement onto every consumer, whether or not the
consumer ever allocates. TAIRiX has real consumers that cannot satisfy it —
three of the four architecture ports allocate nothing in production and run
from CPU reset, the boot console and the early-boot audit ring allocate
nothing, and the staged first-party loader (`plans/BOOTLOADER.md`) cannot. A
cargo feature cannot express this: features unify per build, so a single
heap-backed consumer in the same invocation re-enables `alloc` for everyone.
Only a separate crate makes "the pre-heap layer cannot allocate" an invariant
the dependency graph enforces rather than a convention. It holds `ArrayVec`,
`ArrayString`, `RingBuf`, `SecretRing`, `IntrusiveList`, `BitSet256`, and the
`CapacityError` they refuse through.

**`lib/collections` — the heap-backed containers.** Depends on `lib/inline`,
for the inline half of `SmallVec` and the shared `CapacityError`; on
`lib/hash`; on `lib/cpuops` and the `lib/abi` capability vocabulary it gates
on, for the hash table's group-scan ops table (§4.1); and on `lib/sync`, for
the one-shot cell that holds the resolved scan and for the concurrent tier's
atomics. Nothing else. Neither crate ever depends on `kernel/*`, `drivers/*`,
or `userland/*`, so the existing layering holds unchanged.

**Which crate a new container goes in** is decided by one question, asked
once: can it allocate? If it can, it is `lib/collections`; if it cannot, it is
`lib/inline`. That bisects some later tiers — C9's `SpscRing<T, N>` is
allocation-free while its `MpscQueue<T>` is not — and that is the intended
answer, because the allocation boundary is the one a consumer's crate graph
actually feels.

---

## 2. The rules every container in these crates obeys

These are the acceptance criteria, not aspirations. A container that misses
one is not done.

1. **No operation that can fail may panic.** Every allocating operation has
   a fallible form returning `Result` — `try_insert`, `try_reserve`,
   `try_push`, `try_with_capacity` — because allocation failure is a value,
   not a panic. No `Index`/`IndexMut` impl exists on any map: `map[key]`
   panicking on a missing key has no place in a kernel. Infallible forms
   exist only where they genuinely cannot fail (`get`, `remove`, `len`).
2. **No allocation on any read path, ever.** Lookup, iteration, and range
   query allocate nothing. Growth is amortised and off the hot path.
3. **No fixed capacity ceiling.** Every heap-backed container grows from
   discovered resources and fails closed only on genuine exhaustion. A
   const-generic capacity is permitted **only** where the container is
   deliberately allocation-free (`ArrayVec`, `RingBuf`, `BitSet256`) and the
   bound is the caller's, chosen at its use site.
4. **Deterministic iteration where a consumer depends on it.** Hash
   containers iterate in unspecified order and say so; anything whose output
   is compared, logged, or reproduced uses an ordered container.
5. **Secret hygiene is the holder's job, not the container's.** A container
   does not scrub its own freed slots — reuse inside one address space is
   not a security boundary. A holder of a key, credential, or capability
   token stores a zeroizing value type. This is the rule `lib/rt`'s heap
   already states; there is not a second one.
6. **Every `unsafe` block carries its invariant and a test that exercises
   it**, and no `unsafe` escapes its crate's safe API. The unsafe surface is
   confined to three places — the open-addressing table (`lib/collections`),
   the inline slot arrays (`ArrayVec`, `RingBuf` and its `SecretRing` scrub,
   all `lib/inline`), and the concurrent tier — and each is covered by proptest models, and by loom for
   the concurrent tier. `SmallVec`'s spill needs none of its own: it is an
   enum over `ArrayVec` and `Vec`, so the transition is an ordinary move
   through the owning iterator. `ArrayString` needs none either: a string's
   bytes are always initialised, so it holds a plain `[u8; N]`, which is also
   what lets it be `Copy` and be lifted out from under a lock inside a record.
   **`IntrusiveList` needs none, which is why it is index-addressed rather
   than pointer-linked**: a link holds an index into the caller's store and
   every one is bounds-checked, so a corrupted link is a refused splice rather
   than a wild store. That is the stronger property for a free list a stray
   write can reach, and it is the representation both `LruMap` and `TimerWheel`
   want anyway, since their nodes live in a growable arena a reallocation
   moves.
7. **Untrusted keys are a threat surface.** Every container that accepts
   attacker-influenced keys or lengths gets a fuzz harness in the per-PR
   `--quick` set and the nightly soak.
8. **Cache-shaped containers integrate the existing pressure model.** An
   `LruMap` used as a cache reports through `lib/reclaim`'s budget, band and
   ledger surface. There is no second pressure model.

---

## 3. Hash seeding, and what it is defending

A hash table over attacker-chosen keys degenerates from O(1) to O(n) per
lookup if the attacker can predict which keys collide. The keys TAIRiX
exposes to that are real: filenames from a mounted foreign volume, DNS
names, HTTP headers, network 5-tuples, IPC method names, and bundle
identifiers. `SipHash13` under a key the attacker cannot observe removes the
attack; a fixed key does not.

* The key is 128 bits drawn from the kernel CSPRNG output reserve, published
  **once** per boot in the kernel (beside the per-boot identifier, from the
  same reserve, audited as `HashKeyPublished` / `HashKeyUnavailable`) and
  **once per process** through `tairix_rt::hash_seed()`, so no cross-process
  collision oracle exists and a compromise of one process does not hand an
  attacker another's table layout.
* The per-process draw is **on demand, not at `_start`**. Drawing eagerly puts
  a syscall in every program's entry path to serve the few programs that hash
  attacker-chosen input, and every EL0 test fixture's syscall allow-list has
  to carry it — 22 QEMU verticals failed on exactly that. The key is still
  unpredictable and still published exactly once; only the moment it is drawn
  moves to the first time it is wanted. The kernel's draw stays eager: it is
  once per boot, not once per process, and must precede the first syscall
  that hashes.
* `published()` reports whether a key exists, without drawing one. A container
  constructed unseeded is usable for kernel-assigned keys and refuses
  construction for the untrusted-key case rather than silently using a
  predictable key — fail closed.
* A consumer that is *not* an authority decision and must keep working on a
  platform whose CSPRNG never seeded (`riscv64` and `wasm32` expose no
  entropy source yet) names `HashSeed::UNKEYED`, so the fallback is a
  reviewable choice at the use site rather than a silent default. The two
  such sites — the futex bucket index and the bond flow hash — each report
  the unkeyed state, through the boot audit log and on `stderr`
  respectively.
* `FastHash` is never the default and never correct for untrusted keys. Its
  rustdoc says so, and every use site of it names it explicitly, so the
  choice is visible in review.

---

## 4. Inventory

Each entry names the abstraction, the property that makes it worth having,
and the callers that already need it. A container with no identified caller
is not in this plan — see §6.

### 4.1 Hash containers

| Type | Guarantee | Replaces |
|---|---|---|
| `HashMap<K, V, S>` | O(1) expected lookup/insert/remove, no allocation on lookup | `BTreeMap` used as an unordered index across `kernel/*`, `lib/*`, `drivers/*` |
| `HashSet<T, S>` | as above | `BTreeSet` used as an unordered set. **No such site exists yet**: C1 surveyed every in-tree `BTreeSet` and each is either order-load-bearing (`kernel/sec`'s per-process thread set fans signals out in ascending id order; `kernel/sched/cfq`'s run queue is keyed on virtual runtime) or a handful of entries, where the ordered set is cheaper and a conversion would worsen the counters. The type is a zero-duplication wrapper over `HashMap<T, ()>`; C10 either lands its first caller or deletes it |

`S` carries **no default**, though the two rows above once spelled one. A
defaulted hasher is a `Default`-constructible hasher, which is precisely the
silently-predictable key §3 refuses; requiring it at the construction site is
what makes the choice visible in review.

Open addressing with SIMD control-byte groups (the Swiss-table layout): one
metadata byte per slot plus the entry, probed a group at a time. Chosen over
chaining for cache behaviour and over `BTreeMap` for both speed and
footprint — a `BTreeMap` pays pointer-chasing per level and partial node
fill, which `drivers/filesystem/arxfs/src/dedupe.rs` already has a comment
apologising for.

* **Footprint target: ≤ 1.15 × (`size_of::<(K, V)>()` + one control byte) per
  live entry** at the 87.5 % maximum load factor, versus a `BTreeMap` node's
  fill-dependent overhead. The load factor alone costs 8/7 = 1.143, so the
  budget is against the slot *and* its control byte; stated against the slot
  alone it would be unreachable by arithmetic for any entry under ~160 bytes,
  which is not a bar, only a miscount. Achieved: 19.43 bytes for a 16-byte
  entry against a 19.55-byte budget — exactly 8/7 × (entry + 1), there being
  no node, no partial fill, and no side table.
* **Probe target: ≤ 1.5 group probes for a hit at maximum load**, counted
  deterministically (§5), not timed. Achieved: 1.12 with 7 168 live entries
  at the limit, and 1.00 below it — a sixteen-lane group at that density
  usually answers on its first scan. The `BTreeMap` it replaces takes 8.1 key
  comparisons per lookup at 64 entries, 12.2 at 1 000, and 21.7 at 100 000.
* The group scan is a `lib/cpuops` ops table — SSE2, NEON, and a portable
  scalar baseline — selected through the existing capability gate and
  mandatory self-verify. It is not a `cfg(target_arch)` fork, and the
  baseline is what a target without the feature runs.

### 4.2 Sequences

Every row but `SmallVec` is `lib/inline`: they allocate nothing. `SmallVec`
spills, so it is `lib/collections`, built over `lib/inline`'s `ArrayVec`.

| Type | Guarantee | Replaces |
|---|---|---|
| `ArrayVec<T, N>` | fixed capacity, zero allocation, usable in interrupt context | `sysinfod`'s private `heapless_vec`, and the inline slot array the rest of the tier is built on |
| `SmallVec<T, N>` | inline until `N`, then spills to the heap | hot paths that hold 1–4 items and allocate anyway |
| `ArrayString<N>` | `[u8; N]` + length with the UTF-8 invariant held by construction, and `Copy` | ad-hoc `[u8; N]` + length pairs |
| `RingBuf<T, N>` | fixed-capacity circular queue, O(1) both ends | the four hand-rolled rings listed at the top of this plan |

`alloc::VecDeque` already covers the heap-backed ring, so `RingBuf` is
array-backed only. `Vec` and `String` stay `alloc`'s; there is no reason to
re-implement them and doing so would be bloat.

`SecretRing<T, N>` is `RingBuf` for a queue a credential *transits* — a typed
password crossing a console's type-ahead buffer, a key event crossing the
desktop's input channel. It blanks each slot it vacates with a **volatile**
store (a plain assignment to memory nothing reads again is exactly what an
optimiser may discard), blanks the whole store on a change of holder and again
on drop, and offers no `DerefMut`, so the plain ring's non-scrubbing pops
cannot be reached by accident. Because every slot is initialised from
construction and a `Copy` element cannot un-initialise one, `backing_store`
safely reports the whole store — which is how a holder's own test proves the
scrub, an observation a container's safe API otherwise cannot offer.

**`SmallVec` has no in-tree caller yet.** It landed with C2 by decision, with
its debt recorded here exactly as `HashSet`'s is: its intended first consumer
is `lib/geometry`'s `Region` damage list, whose `rects`/`scratch` `Vec<Rect>`
allocate for the one-to-four-rectangle case a compositor damage region almost
always is. That migration belongs with the measurement `plans/FIX-DESKTOP-SPEEDUP.md`'s
damage work will take, not ahead of it. C10 either lands that caller or deletes
the type.

The fixed-capacity vectors deliberately offer **no positional insert**: its two
failure modes — no room, and an index past the end — are unrelated, and no
single error spells both honestly. The tier's rule instead is that a capacity
fault answers with `Result` and an index fault with `Option`, and no operation
carries both.

### 4.3 Indexed and keyed

| Type | Guarantee | Replaces |
|---|---|---|
| `SlotMap<K, V>` | O(1) insert/remove/get, dense stable keys, **generation counter rejects a stale key** | `next_id` counters plus a `BTreeMap`, in `sharedreg`, all three schedulers, and the capability table |
| `IdAllocator` | smallest-free-id allocation and release over a hierarchical bitmap | monotonic `next_id.fetch_add` for pids, fds, IRQ and MSI vectors, port numbers |
| `BitVec` | dynamic-length bitmap with summary levels: find-first-free in O(log n), not O(n/64) | ad-hoc `Vec<bool>` and `Vec<u64>` scans, including `kernel/mem/src/dma.rs`'s `slot_used` |
| `RangeMap<K, V>` / `RangeSet<K>` | non-overlapping intervals; O(log n) lookup, insert, split, and coalesce | the four range maps listed at the top of this plan |
| `RadixTree<V>` | sparse `u64`-keyed index, fixed-depth descent, gang lookup and **tagged iteration** (dirty / writeback) | `arxfs` page cache and write-cache dirty set, the block cache index |
| `LruMap<K, V>` | **O(1)** touch, insert, and evict via an intrusive list plus a hash index | the six LRUs listed at the top of this plan |

`SlotMap`'s generation counter is a security property, not an ergonomic
one: today a wrapped `next_id` silently aliases a live object with a dead
identifier, and the generation turns that into a rejected lookup.

`BitVec`'s summary levels are what §26.7 demands — a 1 GiB machine serving
several 100 TB+ volumes cannot scan a flat allocation bitmap, and it cannot
hold one resident either.

`RadixTree`'s tag bits are what makes a write-back sweep visit only dirty
blocks instead of walking the whole index; `plans/ARXFS-WRITEBACK.md`
describes that sweep and currently has no structure that supports it.

### 4.4 Ordered and time-ordered

`IntrusiveList` is `lib/inline` (it allocates nothing); the other two rows are
`lib/collections`.

| Type | Guarantee | Replaces |
|---|---|---|
| `IntrusiveList` | O(1) unlink **without a search**, no allocation, links live in the element | the per-site free-list link handling; the `BTreeMap`-based wait set is C8's, once the deadline index it needs exists |
| `IndexedHeap<K, P>` | binary heap with **decrease-key** and O(log n) removal by key | `alloc::BinaryHeap`, which supports neither |
| `TimerWheel` | O(1) arm and cancel, O(1) amortised expiry over thousands of timers | per-subsystem deadline bookkeeping |

`TimerWheel` does **not** reintroduce a periodic tick. It is a bucketed
expiry index whose `next_expiry()` is what arms the existing one-shot
hardware timer; the kernel stays tickless and an idle CPU stays unarmed.
Its consumers are the scheduler's preemption deadline, TCP retransmit,
`plans/FIX-IO.md`'s per-request I/O deadlines, and the watchdog.

### 4.5 Concurrent

Built on `lib/sync`'s existing atomics and epoch reclamation, and living in
`lib/collections` beside the sequential containers they mirror.

| Type | Guarantee | Replaces |
|---|---|---|
| `SpscRing<T, N>` | wait-free single-producer / single-consumer, safe from interrupt context | ISR-to-dispatcher hand-off written per site |
| `MpscQueue<T>` | lock-free many-producer / one-consumer, bounded, **fails closed when full** | per-CPU log and console ingest |
| `ConcurrentMap<K, V>` | lock-free read, epoch-reclaimed; readers never block writers | the `BTreeMap`-per-bucket futex table, and read-mostly registries on the syscall path |

`MpscQueue` failing closed rather than growing is deliberate: an unbounded
producer-side queue is how a hostile client exhausts kernel memory (§26.4).

---

## 5. Proving "fast" — counters gate, wall-clock informs

Wall-clock timings are load-dependent, so per the charter's testing rules
they cannot be a pass/fail threshold and no test asserts an elapsed time —
a timing that varies with host load is the flaky test the charter forbids,
not a gate. This plan follows the
existing house split, the one `tools/xtask/src/commands/bench.rs` already
uses:

* **The gates are deterministic work counters**, asserted by ordinary unit
  tests: probes per lookup, key comparisons per lookup, node reach per
  operation, rehashes per N inserts, bytes resident per live entry,
  allocations per operation (zero on every read path). These are
  reproducible on any machine and are what a regression actually shows up
  in.
* **`cargo xtask bench` gains a `collections` family** producing
  nanoseconds per operation against the *production* entry points, through
  the same `lib/cpuops::BenchHarness` — bounded, median-of-rounds,
  `black_box`ed. It is evidence for a completion report, never a gate.
* **Comparative evidence is part of each migration increment.** An
  increment that replaces a `BTreeMap` with a `HashMap`, or an O(log n)
  eviction with an O(1) one, reports the counter delta it achieved. A
  migration that does not improve its counters has not justified itself and
  is reverted.
* **A scaling test at the §26.7 floor**, not just in the abstract: the
  `BitVec`, `RadixTree`, and `LruMap` verticals run against a simulated
  1 GiB-RAM machine with several 100 TB+ volumes' worth of keys, asserting
  bounded resident bytes and growth-then-fail-closed rather than a panic.

An `unsafe`-heavy container needs an undefined-behaviour oracle. `cargo xtask
miri` is it: a registry of the crates whose safety rests on a hand-written
`unsafe` core, interpreted under Stacked Borrows with strict provenance, in
`ci` and in `ci-long`'s deterministic-gate set. Each listed crate scales its
own sweeps down under `cfg(miri)` — the oracle is hunting undefined behaviour,
which one pass over each code path already exposes, and the wide input search
belongs to the ordinary and budgeted runs.

---

## 6. Deliberately excluded, and why

Listing these is part of the plan: "everything an OS expects" is not
"everything that exists", and each of the following was considered and
rejected for a stated reason.

* **`IndexMap` (insertion-ordered map)** — `BTreeMap` already gives every
  in-tree consumer the deterministic order it needs.
* **String interner, Bloom filter, skip list, `LinkedList`** — no consumer
  in the tree. Adding them would be speculative surface.
* **A general LPM trie** — `lib/net/src/route.rs` has one and is its only
  consumer. It moves into `lib/collections` when a second consumer appears,
  not before.
* **The ARXFS on-disk B-tree and allocation map** — these are the on-disk
  *format*, fixed by `docs/src/filesystem/arxfs-spec.md`. They are bounds,
  not capacities, and are not touched.
* **`lib/abi/src/driver/net_ring.rs`** — a shared-memory wire ABI between
  the kernel and a user-space NIC driver, not a container.
* **The xHCI producer ring (`lib/usb/src/ring.rs`)** — a hardware-defined
  structure the controller reads.
* **Per-CPU storage** — the Arch HAL owns it (§17.2); a container crate must
  not grow a second one.
* **`lib/kalloc`'s three pointer-linked lists** (free blocks, grown regions,
  partial slab pages) **and `kernel/mem/src/kvslots.rs`'s record chains** —
  these look like `IntrusiveList` callers and are not. Their links live *in
  the memory being managed*: a free block's own payload, a slab page's first
  object slot, a record carved out of a drawn frame. There is no array to
  index into, so they are addressed by pointer because the boundary-tag and
  slab *layouts* are, and that layout is a format rather than a container.
  Converting them would also point the global allocator at a crate that
  itself allocates — `lib/kalloc` is deliberately a leaf over `core` and
  stays one. Recorded here so a later sweep does not "fix" them into a
  defect.
* **`kernel/sec/src/captable.rs`'s three `BTreeMap`s** — these look like
  hash-map candidates and are not. The System Information API pages a
  consistent process list off the ascending `ProcessId` iteration order, so
  the order is load-bearing and the ordered map is the correct structure.
  Recorded here so a later sweep does not "fix" it into a defect.
* **An augmented order-statistic tree for EEVDF** — EEVDF is currently the
  only candidate consumer, so it stays scheduler-internal until a second
  appears. Whether EEVDF's `BTreeMap` run queue should become one is a
  question for `plans/WIRING.md`, assessed but not answered here.

---

## 7. What each increment migrates and deletes

An increment lands its engine **and** converts its callers **and** deletes
the implementations it replaces, in one change. It is done only when no copy
of what it replaced remains in the tree; leaving both is the duplication the
plan exists to remove. Each carries its unit tests, its proptest model where
it holds an invariant, its fuzz harness where it takes untrusted keys, its
counter gates, its rustdoc, and its `docs/src/lib/` page.

| # | Deletes |
|---|---|
| C0 | **done.** Six hand-rolled hashes: the FNV-1a folds in `lib/pagezero`'s self-verify fingerprint, `lib/net/src/iface.rs`'s and `lib/net/src/stack.rs`'s multicast revision counters, `kernel/tairix-kernel`'s build-provenance id, and `lib/fontface`'s rasterisation golden; and the unkeyed Fibonacci mixer in `kernel/core/src/futex.rs`'s bucket index |
| C1 | **done.** `kernel/mem/src/dma.rs`'s `allocations` `BTreeMap`, whose only iteration collected keys purely to avoid mutating while iterating, is now a `HashMap` under `BuildFastHash` — the keys are that allocator's own page-aligned window addresses and the window is private to one process, so a caller steering its own allocations can only lengthen its own probes. The carve reserves the record slot beside `ensure_slots`, before any frame is taken, so a bookkeeping refusal fails with nothing to roll back. **A `BTreeMap` converts only where its key order is provably not depended on** — that judgement is per-site, made at migration time, and a site that pages, compares, or logs in key order stays ordered |
| C2 | **done.** All five: `sysinfod`'s `heapless_vec` (an `ArrayVec` whose bound now refuses an over-long fixture loudly instead of silently dropping records); `seat.rs`'s `ChannelRing` and `console.rs`'s `InputRing`, both now `SecretRing`, which also gives the console the drop-time scrub it never had and makes both scrubs volatile where the console's were plain; `boot_audit_ring.rs`'s `RingState`, where `Slot` and `TailRecord` collapsed into one type over an `ArrayString` — deleting the `[u8; 120]`+length pair, the `Level::from_u8` and `u16::try_from` fail-safes the round trip needed, `wrap_index`, and `truncate_on_char_boundary`; and `lib/log`'s `BootRing`, now record framing and eviction-loss accounting over a `RingBuf<u8, N>`, with its byte-at-a-time copies replaced by at most two `copy_from_slice` runs and its borrowed arena by a caller-chosen `N` — so `BufferTooSmall` at construction became a build error and the ring is `const`-constructible for a pre-allocator `static`. **One consequence of that last conversion was missed, and C3 closes it:**
giving `lib/log` a dependency on a container crate that links `alloc` put a
`#[global_allocator]` requirement on every consumer of a *log-bearing
architecture port* — seventeen freestanding verticals that allocate nothing
among them. C3's split (§1) removes the requirement at the root rather than
satisfying it seventeen times: `lib/log` now depends on `lib/inline`, which
links no allocator, so those binaries have no `alloc` in their graph at all |
| C3 | **done.** The tier was **split into two crates** (§1): `lib/inline` for the containers that allocate nothing and `lib/collections` for the heap-backed ones, so `lib/log`, `lib/caps`, the boot console and three of the four architecture ports link no allocator. `kernel/mem/src/frame.rs`'s hand-written per-order free lists: the `FrameNode` `prev`/`next` pair, the `usize::MAX` `NIL` sentinel, the `free_heads` array, and both splice bodies are gone, replaced by one `IntrusiveList` per order over a single `Vec<Link>` indexed by slot. The link is the same two words the old node was, so the per-frame overhead is unchanged. The `blk_order` tag array stays and earns its keep: one store carries every order's list, so it is what says *which* list a registered head is on — the knowledge a shared store cannot hold in the links without a word per node. Two silent-corruption paths closed with it: re-registering a block, and unlinking one whose tag and links disagree, are now `AllocError::InvariantViolation` where the first was undetected and the second a release-mode `debug_assert` above a frame handed out twice. The counter gate is nodes reached: a mid-list unlink reaches the departing node and its two neighbours and writes exactly those three links, identically over three nodes and over ten thousand |
| C4 | all six LRUs: `block_cache`, `transform_cache`, `launch_cache`, `fscache`, `arxfs/dedupe`, and the index inside `lib/reclaim/src/cache.rs`; **and the O(n) `lib/net/src/neigh.rs` scan** |
| C5 | `kernel/mem/src/anon_window.rs`, `kernel/mem/src/mmio.rs`, both maps in `kernel/core/src/aspace.rs`, `drivers/filesystem/arxfs/src/runs.rs` |
| C6 | the `next_id` counters in `sharedreg` and all three schedulers; `dma.rs`'s `slot_used`; flat bitmap scans |
| C7 | `arxfs/pagecache.rs`, `arxfs/wcache.rs`'s dirty set, the block-cache index |
| C8 | per-subsystem deadline bookkeeping; wires `plans/FIX-IO.md`'s per-request deadlines |
| C9 | `kernel/core/src/futex.rs`'s `BTreeMap`-per-bucket table, which becomes one `ConcurrentMap` — migrated once, here, rather than through an interim `HashMap` under the same bucket locks; and per-CPU log and console ingest |
| C10 | `docs/src/lib/collections.md` and `docs/src/lib/hash.md` rewritten; stability tiers set in both `README.md`s; a `deps-check` rule that fails a new hand-rolled container in `kernel/*`, `drivers/*`, or `userland/*` |

---

## 8. What "done" means for this plan

The plan is done when every ledger row reads `done` and: both crates exist
with the §4 inventory and the §2 rules hold for every type in it; no
hand-rolled equivalent remains anywhere in the tree; each migration
increment has reported its counter improvement; `cargo xtask miri` is in
`ci`; and the §6 exclusions still hold — nothing on that list crept in
without a second consumer.
