# `tairix-collections`

The **heap-backed** `no_std` containers TAIRiX runs on that `core` and `alloc`
have no answer for. `alloc` already supplies `Vec`, `String`, `BTreeMap`,
`BTreeSet`, `VecDeque`, and `BinaryHeap`; none of them is re-implemented here,
and a container with no caller in the tree is not added.
`plans/COLLECTIONS.md` is the ledger of what has landed and what is still to
come.

The containers that allocate *nothing* live in [`tairix-inline`](./inline.md) —
a crate that links neither this one nor `alloc`, so the layer running before a
heap exists can use them. This crate depends on it for the inline half of its
`SmallVec`.

`HashSet` and `SmallVec` are the two types here without an in-tree caller yet.
Every remaining `BTreeSet` is either order-load-bearing (`kernel/sec`'s
per-process thread set fans signals out in ascending id order) or small enough
that the ordered set is the cheaper structure; `SmallVec`'s first consumer is a
compositor damage list, which is measured rather than assumed. Both arrive with
a later tier.

## Inventory

| Type | Guarantee | Stands in for |
|---|---|---|
| `HashMap<K, V, S>` | expected O(1) lookup, insert, and remove; one control byte and one `(K, V)` slot per bucket, no per-entry node | a `BTreeMap` used where the key order was never wanted |
| `HashSet<T, S>` | the same, over a zero-sized value | a `BTreeSet` used as an unordered set |
| `SmallVec<T, N>` | inline to `N`, then one spill to the heap | a hot path that holds a handful of elements and allocates anyway |

## The rules every container obeys

1. **Nothing that can fail panics.** Every allocating operation has a fallible
   form returning `TryReserveError` — `try_insert`, `try_reserve`,
   `try_with_capacity_and_hasher`. No map has an `Index` implementation,
   because a subscript that panics on a missing key has no place in a kernel.
2. **No allocation on a read path.** Lookup, iteration, and removal allocate
   nothing; growth is amortised and off the hot path.
3. **No fixed capacity ceiling.** A container here grows on demand and fails
   closed only on genuine exhaustion; a caller-chosen compile-time bound is the
   other crate's business.
4. **Order is unspecified unless the container says otherwise.** A hash
   container's iteration order varies with the hash key, the insertion
   history, and the capacity, so anything compared, logged, paged, or
   reproduced uses an ordered container.
5. **Secret hygiene is the holder's job.** A container does not scrub the slots
   it frees — reuse inside one address space is not a security boundary — so a
   holder of a key, credential, or capability token stores a value type that
   zeroes itself on drop, exactly as the userland heap in `lib/rt` already
   requires.

## Choosing a hasher

There is deliberately no default construction: `HashMap::with_hasher` takes the
`BuildHasher` explicitly, so the choice is visible wherever a map is created.

```rust,ignore
// Keys an attacker can choose or influence.
let map = HashMap::with_hasher(BuildSipHash13::keyed()?);

// Keys the kernel assigns itself.
let map = HashMap::with_hasher(BuildFastHash::new());
```

`BuildSipHash13::keyed` refuses to build until the per-boot key has been
published (see [`tairix-hash`](./hash.md)). Hashing attacker-chosen keys under
a predictable key lets an attacker pick a set that all collide, turning every
lookup into a linear scan; the keyed pseudo-random function removes the attack,
and refusing to construct is the fail-closed answer to being asked before the
key exists.

## The table

Open addressing over sixteen-lane control-byte groups. One byte beside each
slot holds either `EMPTY`, `DELETED`, or the key's seven-bit tag, and a probe
loads a whole group and asks three questions of it at once: which lanes carry
this tag, which are empty (the probe chain ends there), and which are free to
take an insertion. Lookup and insertion share that one primitive.

Groups are **aligned**: probing steps whole groups rather than a byte offset
into one. The control array therefore ends exactly at `buckets` bytes with no
wrap-around mirror of its head, and there is no paired-write invariant to
maintain on every control update — the simplification that keeps the `unsafe`
core small.

Two invariants carry the removal path:

* A group holding an empty lane has no probe chain passing *through* it.
  Insertion maintains it by only stepping past a group with no empty lane.
* Removal therefore writes `EMPTY` only into a group that already had one, and
  `DELETED` otherwise, which keeps every chain through that group intact.

The load-factor limit is seven eighths. When it is reached — by live entries,
by tombstones, or by both — the table rebuilds into a fresh allocation sized
for the live set, which grows it and clears its tombstones in one pass. A
steady live set therefore settles on a steady footprint no matter how much
churn passes through it.

## The group scan is a dispatched ops table

The scan is a [`lib/cpuops`](./cpuops.md) family: an SSE2 candidate, a NEON
candidate, and a portable word-at-a-time baseline. Which one runs is decided
once per boot (`kernel/core`'s `resolve_accelerated_ops`) through the
capability gate and the mandatory self-verify, so a vector instruction is never
reached on a core that lacks it, and a candidate that disagrees with the
baseline on any vector cannot be selected. It is never benchmarked: the control
bytes it reads are tags derived from the per-boot hash key, and a benchmark is
a timing measurement over exactly that.

A target whose vector unit is off compiles no candidate at all and calls the
baseline directly, paying neither the resolved-cell load nor an indirect call.
That is every freestanding target but `aarch64`: the `x86_64` kernel target is
soft-float and SSE-disabled, and neither riscv64 nor wasm32 has a candidate.
The build script makes that decision from the target's own feature set, so no
target-conditional predicate appears in the crate source.

The portable baseline's byte-equality test is *exact*. The shorter
`(x - 1) & !x` zero-byte trick lets one lane's borrow forge a match in the
next, which would make the baseline disagree with the exactly-comparing vector
candidates and so make the self-verify's bit-identity conditional on the input.

## Measurement

Probe depth, not elapsed time, is what a hash table's performance is, and
unlike a stopwatch it is reproducible on any machine — so it, and not a timing,
is what the crate's tests gate. `HashMap::probe_groups` reports the control
groups a lookup examines and `allocated_bytes` the resident footprint. At the
maximum load factor:

| Counter | Gate | Measured |
|---|---|---|
| Control-group scans per hit | ≤ 1.5 | 1.12 with 7 168 live entries; 1.00 below the limit |
| Bytes per live entry | ≤ 1.15 × (entry + control byte) | 19.43 for a 16-byte entry, against a 19.55 budget — exactly 8/7 |

Against the `BTreeMap` these replace, over page-aligned `u64` keys of the shape
the DMA-window index uses:

| Live entries | `BTreeMap` key comparisons per lookup | Control-group scans per lookup |
|---|---|---|
| 64 | 8.1 | 1.000 |
| 1 000 | 12.2 | 1.001 |
| 100 000 | 21.7 | 1.032 |

The ordered map's cost grows with the log of the population, and each level is
a pointer chase to a node whose fill varies between a half and full. The
table's does not move, and its footprint is what the load factor alone
accounts for.

## Tests

Unit tests live next to the code. Beyond the ordinary map and set behaviour
they cover the portable scan against a naive lane-by-lane reference over every
adjacent-byte pair, every compiled vector candidate against the same reference
at every lane, churn through tens of thousands of tombstones with every
survivor still reachable and the footprint still bounded, and one drop per
value ever inserted across growth, overwrite, removal, retention, and an
abandoned owning iterator.

The intrusive list is gated on the same kind of number: a mid-list unlink
reaches the departing node and its two neighbours and writes exactly those
three links, identically over three nodes and over ten thousand. That is what
"constant time, no search" is, stated as something a regression shows up in.

The sequence tier's tests hold each container to the same bar: one drop per
element ever taken, across a bulk push, a wrapped drain, an eviction and a
spill; a footprint that is the elements plus their indices and no heap block;
and — for `ArrayVec::retain`, the one operation that moves elements out and back
— that a predicate which unwinds leaves the vector describing exactly what it
has written back, so the unswept tail leaks where a double drop would be
unsound.

`tests/fuzz_collections.rs` drives the map against a plain association list
over deliberately colliding key streams, `tests/fuzz_sequences.rs` drives the
sequence tier against naive models over arbitrary lengths and text — the
lengths are attacker-influenced by design, since a boot audit line carries
caller-controlled text into an `ArrayString` and a console ring takes whatever
a keyboard produces — and `tests/fuzz_intrusive.rs` drives two lists over one
shared store against naive order models, since a free list's link/unlink order
is whatever a process's allocation pattern makes it. All under
`cargo xtask fuzz`, and
`cargo xtask miri` interprets the whole suite under the undefined-behaviour
oracle — the table's `unsafe` core is why that stage exists. A test suite says
what the code computes; only an interpreter says whether a raw pointer stayed
in bounds, whether a slot was initialised before it was read, and whether two
`&mut` ever aliased.

## Stability

**experimental.** The public API may change until the first tagged release.
