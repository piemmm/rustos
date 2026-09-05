# tairix-collections

The `no_std` containers TAIRiX runs on that `core` and `alloc` have no answer
for. `alloc` already supplies `Vec`, `String`, `BTreeMap`, `BTreeSet`,
`VecDeque`, and `BinaryHeap`; none of them is re-implemented here.

| Type | Guarantee |
|---|---|
| `HashMap<K, V, S>` | expected O(1) lookup / insert / remove, one control byte + one `(K, V)` slot per bucket, no per-entry node |
| `HashSet<T, S>` | the same, over a zero-sized value |
| `ArrayVec<T, N>` | up to `N` elements inline, allocating nothing |
| `SmallVec<T, N>` | inline to `N`, then one spill to the heap |
| `ArrayString<N>` | up to `N` bytes inline, the UTF-8 invariant held by construction |
| `RingBuf<T, N>` | a fixed-capacity circular queue, constant time at both ends |
| `SecretRing<T, N>` | the same, scrubbing each slot it vacates |
| `IntrusiveList` | a doubly-linked list over nodes the caller owns: constant-time unlink of any node, no search, no allocation |
| `BitSet256` | a fixed 256-bit set: constant-time membership, allocation-free |

`plans/COLLECTIONS.md` is the ledger of what has landed and what is still to
come. `HashSet` and `SmallVec` are the two types here with no in-tree caller
yet: every remaining `BTreeSet` is either order-load-bearing or small enough
that the ordered set is the cheaper structure, and `SmallVec`'s first consumer
is a compositor damage list that is measured rather than assumed. Both arrive
with a later tier.

## The rules every container here obeys

1. **Nothing that can fail panics.** Every allocating operation has a fallible
   form returning `TryReserveError`. No map has an `Index` implementation: a
   subscript that panics on a missing key has no place in a kernel.
2. **No allocation on a read path.** Lookup, iteration, and removal allocate
   nothing; growth is amortised.
3. **No fixed capacity ceiling.** A heap-backed container grows on demand and
   fails closed only on genuine exhaustion. A const-generic capacity appears
   only where the container is deliberately allocation-free and the bound is
   the caller's own, chosen at the use site.
4. **Order is unspecified unless the container says otherwise.** A hash
   container's iteration order varies with the hash key and the insertion
   history; anything compared, logged, or reproduced wants an ordered
   container.
5. **Secret hygiene is the holder's job.** A container does not scrub the slots
   it frees — reuse inside one address space is not a security boundary — so a
   holder of a key, credential, or capability token stores a value type that
   zeroes itself on drop, exactly as `lib/rt`'s heap already requires. A value
   that merely *transits* a long-lived kernel buffer is the one exception, and
   it has its own type: `SecretRing` scrubs every slot it vacates with a
   volatile store, so the write cannot be optimised away, and offers no
   `DerefMut` so the plain ring's non-scrubbing pops stay out of reach.
6. **A capacity fault answers with `Result`, an index fault with `Option`, and
   no operation carries both.** Hence no positional insert on the
   fixed-capacity vectors: its two failure modes are unrelated and no single
   error spells them honestly.

## The intrusive list

`IntrusiveList` is a three-word header; the links live in the caller's own
store — a `[Link]`, or its own slot type with a link field — and every
operation takes it. That is what an owning container cannot offer: any node
leaves the list in constant time from its index alone, with no search and no
allocation, so a buddy allocator can splice out an arbitrary free block on a
merge and a recency list can move an arbitrary entry to the front. Several
lists may share one store, which is exactly what a per-order free-list array
is.

It is **index-addressed, and holds no `unsafe`.** A pointer-linked list is
what a C kernel writes, and it is also how a stray link becomes a wild store;
here every index is bounds-checked, so a corrupted link is a refused splice
(`LinkError::Corrupt`) rather than memory corruption. A link is two words —
the on-a-list state is encoded in the neighbour ends rather than a third
field, because a free list with one link per page cannot afford one — which
reserves the two index values above `MAX_INDEX`, far above any store that has
a representable size.

Nothing is written until every node a splice touches is known to exist, so a
refused operation leaves the list and the store exactly as they were. What the
list can check it does: a node is on at most one list, an unlink of a node
claiming an end this list does not hold is refused, and every neighbour must
link back. What it cannot check it says: an *interior* node of a sibling list
sharing the store is indistinguishable from one of its own, which would need a
list identity in every link, and the caller sharing a store is the one that
already knows (`kernel/mem`'s per-order tag array is that knowledge).

## Choosing a hasher

There is no default. `HashMap::with_hasher` takes the `BuildHasher`
explicitly, so the choice is visible wherever a map is created:

* `tairix_hash::BuildSipHash13::keyed()` for keys an attacker can choose or
  influence — a filename off a foreign volume, a DNS name, a 5-tuple. It
  refuses to build until the per-boot key is published, because a predictable
  key lets an attacker pick a set that all collide and turns every lookup into
  a linear scan.
* `tairix_hash::BuildFastHash` for keys the kernel assigns itself.

## The table

Open addressing over sixteen-lane control-byte groups. One byte per slot holds
either `EMPTY`, `DELETED`, or the key's seven-bit tag, and a probe examines a
whole group at once: which lanes carry this tag, which are empty (the chain
ends there), and which are free to take an insertion.

Groups are **aligned** — probing steps whole groups, never a byte offset into
one — so the control array ends exactly at `buckets` bytes with no wrap-around
mirror of its head, and there is no paired-write invariant to maintain on every
control update.

The scan itself is a `lib/cpuops` ops table: an SSE2 candidate, a NEON
candidate, and a portable word-at-a-time baseline. Which one runs is decided
once per boot through the capability gate and the mandatory self-verify, so a
vector instruction is never reached on a core that lacks it and a candidate
that disagrees with the baseline on any vector cannot be selected. A target
whose vector unit is off — the SSE-disabled `x86_64` kernel target, riscv64,
wasm32 — compiles no candidate at all and calls the baseline directly, paying
neither the resolved-cell load nor an indirect call.

## Measurement

Probe depth, not elapsed time, is what a hash table's performance is, and
unlike a stopwatch it is reproducible on any machine. `HashMap::probe_groups`
reports the control groups a lookup examines and `allocated_bytes` the resident
footprint; the crate's tests gate both at the maximum load factor:

* **≤ 1.5 control-group scans per hit.** Measured at 1.12 with 7 168 live
  entries at the 87.5 % load factor: a sixteen-lane group at that density
  usually answers on its first scan. Below the limit it is 1.00.
* **≤ 1.15 × (entry + control byte) per live entry.** Measured at 19.43 bytes
  for a 16-byte entry, against a 19.55-byte budget — exactly the 8/7 the load
  factor imposes, because there is no node, no partial fill, and no side
  table.

## Tests

Unit tests live next to the code. Beyond the ordinary map and set behaviour
they cover: the portable scan against a naive lane-by-lane reference over every
adjacent-byte pair (a borrow crossing a lane boundary is the classic way a
word-at-a-time equality test forges a match); every compiled vector candidate
against the same reference at every lane; churn through tens of thousands of
tombstones with every survivor still reachable and the footprint still bounded;
and one drop per value ever inserted, across growth, overwrite, removal,
retention, and an abandoned owning iterator.

The sequence tier is held to the same bar: one drop per element ever taken
across a bulk push, a wrapped drain, an eviction and a spill; a footprint that
is the elements plus their indices and no heap block; and a `retain` predicate
that unwinds leaving the vector describing exactly what it wrote back.

The intrusive list is gated the same way: a mid-list unlink reaches the
departing node and its two neighbours and writes exactly those three links,
over three nodes and over ten thousand alike, which is what "constant time, no
search" means as a reproducible number.

`tests/fuzz_collections.rs` drives the map against a plain association list
over deliberately colliding key streams, `tests/fuzz_sequences.rs` drives the
sequence tier against naive models over arbitrary lengths and text, and
`tests/fuzz_intrusive.rs` drives two lists over one shared store against naive
order models — all under `cargo xtask fuzz`; `cargo xtask miri` interprets the
whole suite under the undefined-behaviour oracle — the inline slot arrays are
the reason that stage exists.

## Stability

**experimental.** The public API may change until the first tagged release;
nothing here is frozen yet.
