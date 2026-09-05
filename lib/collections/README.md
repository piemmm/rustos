# tairix-collections

The **heap-backed** `no_std` containers TAIRiX runs on that `core` and `alloc`
have no answer for. `alloc` already supplies `Vec`, `String`, `BTreeMap`,
`BTreeSet`, `VecDeque`, and `BinaryHeap`; none of them is re-implemented here.

The containers that allocate *nothing* live in
[`tairix-inline`](../inline/README.md) — a crate that links neither this one
nor `alloc`, so the layer running before a heap exists can use them. This crate
depends on it for the inline half of its `SmallVec`.

| Type | Guarantee |
|---|---|
| `HashMap<K, V, S>` | expected O(1) lookup / insert / remove, one control byte + one `(K, V)` slot per bucket, no per-entry node |
| `HashSet<T, S>` | the same, over a zero-sized value |
| `LruMap<K, V, S>` | the same table with a recency order through it: expected O(1) lookup, touch, and eviction of the coldest entry |
| `RangeMap<K, V>` | disjoint half-open ranges, each an identity: O(log n) covering lookup, insertion refusing overlap, and first-fit placement over the gaps |
| `RangeSet<K>` | the same storage canonicalised — insertion absorbs what it touches, removal splits what it cuts — so the entry count is one per contiguous run |
| `SmallVec<T, N>` | inline to `N`, then one spill to the heap |

`plans/COLLECTIONS.md` is the ledger of what has landed across both container
crates and what is still to come. `HashSet` and `SmallVec` are the two types
here with no in-tree caller yet: every remaining `BTreeSet` is either
order-load-bearing or small enough that the ordered set is the cheaper
structure, and `SmallVec`'s first consumer is a compositor damage list that is
measured rather than assumed. Both arrive with a later tier.

## The rules every container here obeys

1. **Nothing that can fail panics.** Every allocating operation has a fallible
   form returning `TryReserveError`. No map has an `Index` implementation: a
   subscript that panics on a missing key has no place in a kernel. The one
   allocation this crate does not control is the ordered tier's: `RangeMap`
   stores its entries in `alloc`'s `BTreeMap`, whose insertion cannot be made
   fallible from outside `alloc`.
2. **No allocation on a read path.** Lookup, iteration, and removal allocate
   nothing; growth is amortised.
3. **No fixed capacity ceiling.** A container here grows on demand and fails
   closed only on genuine exhaustion; a caller-chosen compile-time bound is the
   other crate's business.
4. **Order is unspecified unless the container says otherwise.** A hash
   container's iteration order varies with the hash key and the insertion
   history; anything compared, logged, or reproduced wants an ordered
   container.
5. **Secret hygiene is the holder's job.** A container does not scrub the slots
   it frees — reuse inside one address space is not a security boundary — so a
   holder of a key, credential, or capability token stores a value type that
   zeroes itself on drop, exactly as `lib/rt`'s heap already requires.

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

`SmallVec` is held to the same bar as the table: one drop per element ever
taken across its spill, and an inline footprint carrying no heap pointer.

`tests/fuzz_collections.rs` drives the map against a plain association list
over deliberately colliding key streams and `SmallVec` across its spill
against a `Vec` model, under `cargo xtask fuzz`; `cargo xtask miri` interprets
the suite under the undefined-behaviour oracle, which is what the table's
`unsafe` core needs. A test suite says what the code computes; only an
interpreter says whether a raw pointer stayed in bounds, whether a slot was
initialised before it was read, and whether two `&mut` ever aliased.

## Stability

**experimental.** The public API may change until the first tagged release;
nothing here is frozen yet.
