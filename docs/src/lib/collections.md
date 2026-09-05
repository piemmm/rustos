# `tairix-collections`

The `no_std` containers TAIRiX runs on that `core` and `alloc` have no answer
for. `alloc` already supplies `Vec`, `String`, `BTreeMap`, `BTreeSet`,
`VecDeque`, and `BinaryHeap`; none of them is re-implemented here, and a
container with no caller in the tree is not added. `plans/COLLECTIONS.md` is
the ledger of what has landed and what is still to come.

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
| `ArrayVec<T, N>` | up to `N` elements inline, allocating nothing | an ad-hoc `[T; N]` paired with a length field |
| `SmallVec<T, N>` | inline to `N`, then one spill to the heap | a hot path that holds a handful of elements and allocates anyway |
| `ArrayString<N>` | up to `N` bytes inline, the UTF-8 invariant held by construction | an ad-hoc `[u8; N]` plus length, with the invariant assumed |
| `RingBuf<T, N>` | a fixed-capacity circular queue, constant time at both ends | every hand-rolled type-ahead buffer, diagnostic tail, and driver hand-off ring |
| `SecretRing<T, N>` | the same, scrubbing each slot it vacates | a ring a credential transits, zeroed by hand at each call site |
| `IntrusiveList` | a doubly-linked list over nodes the caller owns: constant-time unlink of any node, no search, no allocation | a hand-written `next`/`prev` index pair with its own sentinel and splice arithmetic |
| `BitSet256` | a fixed 256-bit set: constant-time membership, allocation-free | a process's capability membership (`lib/caps`) |

## The rules every container obeys

1. **Nothing that can fail panics.** Every allocating operation has a fallible
   form returning `TryReserveError` — `try_insert`, `try_reserve`,
   `try_with_capacity_and_hasher`. No map has an `Index` implementation,
   because a subscript that panics on a missing key has no place in a kernel.
2. **No allocation on a read path.** Lookup, iteration, and removal allocate
   nothing; growth is amortised and off the hot path.
3. **No fixed capacity ceiling.** A heap-backed container grows on demand and
   fails closed only on genuine exhaustion. A const-generic capacity appears
   only where the container is deliberately allocation-free and the bound is
   the caller's own, chosen at the use site.
4. **Order is unspecified unless the container says otherwise.** A hash
   container's iteration order varies with the hash key, the insertion
   history, and the capacity, so anything compared, logged, paged, or
   reproduced uses an ordered container.
5. **Secret hygiene is the holder's job.** A container does not scrub the slots
   it frees — reuse inside one address space is not a security boundary — so a
   holder of a key, credential, or capability token stores a value type that
   zeroes itself on drop, exactly as the userland heap in `lib/rt` already
   requires. A value that merely *transits* a long-lived kernel buffer is the
   one exception, and it has its own type: see `SecretRing` below.
6. **A capacity fault answers with `Result`, an index fault with `Option`, and
   no operation carries both.** That is why the fixed-capacity vectors offer no
   positional insert: its two failure modes — no room, and an index past the
   end — are unrelated, and no single error spells them honestly.

## The sequence tier

`ArrayVec` is the inline slot array the tier is built on: `[MaybeUninit<T>; N]`
and a length, dropping exactly its live prefix. `ArrayString` is not built on it
— a string's bytes are always initialised, so it holds a plain `[u8; N]`, which
costs no `unsafe` and lets it be `Copy`, and a record carrying one can be lifted
out from under a lock. `SmallVec` is an enum over an `ArrayVec` and a `Vec`,
which is why its spill needs no `unsafe` of its own: the transition is an
ordinary move through `ArrayVec`'s owning iterator. It never returns inline,
because re-inlining would trade a branch on every later operation for a saving
the growth pattern that caused the spill is unlikely to want.

`RingBuf` keeps its own `[MaybeUninit<T>; N]`, since a ring's occupied region
wraps and an `ArrayVec`'s is a contiguous prefix. Its bulk paths for `Copy`
elements — `push_slice`, `pop_slice`, `peek_slice` — copy in at most two
`copy_from_slice` runs either side of the wrap, where each of the rings it
replaced moved one byte per iteration. `peek_slice` is what lets a
variable-length frame read its header, decide, and only then consume, so a
drainer that cannot accept a record leaves it queued: that is `lib/log`'s
early-boot ring, which is now record framing and eviction-loss accounting over
this one byte ring rather than a second ring of its own.

### `SecretRing`, and why it is a type

A typed password crosses a console's type-ahead queue between the keyboard
driver and the login that reads it; a key event carrying one crosses the
desktop's input channel. Without a scrub the cleartext would sit in a
long-lived kernel buffer for the rest of the boot, well after its reader took
it. `SecretRing` writes a blank over each slot as the element leaves, over the
whole store when its holder changes, and over the whole store again as it is
dropped — zero-on-free for memory that held a credential.

Each scrub is a **volatile** store followed by a compiler fence. A plain
assignment to memory nothing reads again is precisely the store an optimiser may
discard, and discarding it would leave the cleartext in place; the scrub has to
be un-elidable to be real.

It is a type rather than a convention for two reasons. There is no `DerefMut`,
so the plain ring's non-scrubbing pops are out of reach and no later edit can
bypass the scrub by accident — reads reach through `Deref` unchanged. And
because every slot is initialised from construction onward, and a `Copy` element
has no way to un-initialise one, `backing_store` can hand out the whole store,
vacated slots included. That is what lets a holder's own test prove the scrub
left nothing behind, which is not observable through a container's safe API at
all.

## The intrusive list

`IntrusiveList` is a three-word header. The links live in the caller's own
store — a `[Link]`, or its own slot type carrying a `Link` field, reached
through the `LinkStore` trait — and every operation takes it. That is the
property an owning container cannot offer: **any** node leaves the list in
constant time from its index alone, with no search and no allocation. A buddy
allocator splices an arbitrary free block out on a merge; a recency list moves
an arbitrary entry to the front on a touch. Several lists may share one store,
which is exactly what a per-order free-list array is.

There is no ordering policy of its own — the caller picks one by which end it
pushes and pops. `push_back` with `pop_front` is FIFO, the discipline a wait
set wants because it cannot starve a waiter; `push_front` with `pop_back` is
recency, where `move_to_front` is the touch and the eviction candidate is the
back.

### Index-addressed, and no `unsafe`

A pointer-linked list is what a C kernel writes, and it is also how a stray
link becomes a wild store. Here a link holds an index into the caller's store
and every one is bounds-checked, so a corrupted link is a refused splice
(`LinkError::Corrupt`) rather than memory corruption — which is why this
container is in the tier's `unsafe` budget for nothing at all, and why a
`LinkStore` implementation that answers inconsistently can make a list
*wrong* but never unsound.

A link is two words. The on-a-list state is encoded in the neighbour ends
rather than carried in a third field, because rounding the link up to three
words costs a word per node and a free list with one link per page cannot
afford one. That reserves the two index values above `MAX_INDEX` — a bound no
store with a representable size comes near, since a node occupies at least a
link's worth of storage.

### What it checks, and what it says it cannot

Nothing is written until every node a splice touches is known to exist, so a
refused operation leaves the list and the store exactly as they were. Within
that:

* A node is on at most one list: a push of a node another list holds is
  refused outright.
* An unlink is refused when the node is on no list, or when it claims an end
  this list does not hold.
* Every neighbour must link back at the node, and a link naming a node the
  store no longer holds is corruption rather than a caller error.
* Walks are bounded by the length, so links corrupted into a cycle end an
  iteration instead of spinning, and `clear` empties the list whatever the
  chain turns out to be — reporting how many it detached, so a caller
  comparing that against the length it had is what notices.

The one case it *cannot* detect is an **interior** node of a sibling list
sharing the same store: its neighbours do link back at it, and only a list
identity stored in every link would tell the two apart. That word per node is
the cost the two-word link exists to avoid, and the caller sharing a store
already holds the answer — `kernel/mem`'s per-order tag array is exactly that
knowledge, and it is what makes the buddy allocator's `remove_free_block`
name the right list before it asks.

### The first caller

`kernel/mem`'s buddy allocator keeps one list per order over a single link
array indexed by starting frame, and a merge unlinks its buddy from the middle
of a list in constant time. It had hand-written `prev`/`next` fields, a
`usize::MAX` sentinel, and its own head-array splice arithmetic; those are
gone, and two silent-corruption paths went with them. Registering a block
twice, or unlinking one whose order tag and links disagree, are now
`AllocError::InvariantViolation` instead of a release-mode `debug_assert` and
a frame handed out twice.

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
