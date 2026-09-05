# tairix-inline

The allocation-free containers TAIRiX runs on, whose storage lives inline in
something the caller already owns.

**Linking this crate never requires a global allocator.** It depends on
nothing — not even `alloc`. That is the whole reason it exists separately from
[`tairix-collections`](../collections/README.md): the layer that runs before a
heap exists needs containers, and a container crate that links `alloc` forces
an allocator requirement onto every consumer whether or not it ever allocates.
Three of the four architecture ports allocate nothing in production, the boot
console and the early-boot audit ring allocate nothing, and a firmware loader
cannot. They all link this crate.

| Type | Guarantee |
|---|---|
| `ArrayVec<T, N>` | up to `N` elements inline, allocating nothing |
| `ArrayString<N>` | up to `N` bytes inline, the UTF-8 invariant held by construction |
| `RingBuf<T, N>` | a fixed-capacity circular queue, constant time at both ends |
| `SecretRing<T, N>` | the same, scrubbing each slot it vacates |
| `IntrusiveList` | a doubly-linked list over nodes the caller owns: constant-time unlink of any node, no search |
| `BitSet256` | a fixed 256-bit set: constant-time membership |

`plans/COLLECTIONS.md` is the ledger of what has landed across both container
crates and what is still to come.

## The rules every container here obeys

1. **Nothing allocates, ever.** A capacity is the caller's own bound, chosen at
   the use site; reaching it is answered with `CapacityError` holding the
   refused value, never a panic and never a heap block.
2. **A capacity fault answers with `Result`, an index fault with `Option`, and
   no operation carries both.** Hence no positional insert on the
   fixed-capacity vectors: its two failure modes are unrelated and no single
   error spells them honestly.
3. **Secret hygiene is the holder's job.** A container does not scrub the slots
   it frees — reuse inside one address space is not a security boundary — so a
   holder of a key, credential, or capability token stores a value type that
   zeroes itself on drop, exactly as `lib/rt`'s heap already requires. A value
   that merely *transits* a long-lived kernel buffer is the one exception, and
   it has its own type: `SecretRing` scrubs every slot it vacates with a
   volatile store, so the write cannot be optimised away, and offers no
   `DerefMut` so the plain ring's non-scrubbing pops stay out of reach.
4. **Usable from interrupt context.** No allocation and no lock means nothing
   here can block or re-enter an allocator, which is what lets an ISR hand off
   through one.

## The sequence tier

`ArrayVec` is the inline slot array the tier is built on: `[MaybeUninit<T>; N]`
and a length, dropping exactly its live prefix. `ArrayString` is not built on
it — a string's bytes are always initialised, so it holds a plain `[u8; N]`,
which costs no `unsafe` and lets it be `Copy`, so a record carrying one can be
lifted out from under a lock.

`RingBuf` keeps its own `[MaybeUninit<T>; N]`, since a ring's occupied region
wraps and an `ArrayVec`'s is a contiguous prefix. Its bulk paths for `Copy`
elements — `push_slice`, `pop_slice`, `peek_slice` — copy in at most two
`copy_from_slice` runs either side of the wrap, where each of the rings it
replaced moved one byte per iteration. `peek_slice` is what lets a
variable-length frame read its header, decide, and only then consume, so a
drainer that cannot accept a record leaves it queued: that is `lib/log`'s
early-boot ring, which is record framing and eviction-loss accounting over this
one byte ring rather than a second ring of its own.

## The intrusive list

`IntrusiveList` is a three-word header; the links live in the caller's own
store — a `[Link]`, or its own slot type with a link field — and every
operation takes it. Any node leaves the list in constant time from its index
alone, with no search and no allocation, so a buddy allocator can splice out an
arbitrary free block on a merge and a recency list can move an arbitrary entry
to the front. Several lists may share one store, which is exactly what a
per-order free-list array is.

It is **index-addressed, and holds no `unsafe`.** A pointer-linked list is what
a C kernel writes, and it is also how a stray link becomes a wild store; here
every index is bounds-checked, so a corrupted link is a refused splice
(`LinkError::Corrupt`) rather than memory corruption. A link is two words — the
on-a-list state is encoded in the neighbour ends rather than a third field,
because a free list with one link per page cannot afford one — which reserves
the two index values above `MAX_INDEX`, far above any store that has a
representable size.

Nothing is written until every node a splice touches is known to exist, so a
refused operation leaves the list and the store exactly as they were. What the
list can check it does: a node is on at most one list, an unlink of a node
claiming an end this list does not hold is refused, and every neighbour must
link back. What it cannot check it says: an *interior* node of a sibling list
sharing the store is indistinguishable from one of its own, which would need a
list identity in every link, and the caller sharing a store already knows
(`kernel/mem`'s per-order tag array is that knowledge).

## Measurement

Work counters, not elapsed time, are what these are gated on — a counter is
reproducible on any machine where a stopwatch is not. A mid-list unlink reaches
the departing node and its two neighbours and writes exactly those three links,
identically over three nodes and over ten thousand; an `ArrayVec`'s footprint
is its elements plus its length and no heap block.

## Tests

Unit tests live next to the code: one drop per element ever taken across a bulk
push, a wrapped drain and an eviction; a `retain` predicate that unwinds
leaving the vector describing exactly what it wrote back; an `ArrayString` that
never stores a partial character however the text was cut; and a `SecretRing`
that leaves nothing but the blank in a slot it has vacated.

`tests/fuzz_inline.rs` drives the tier against naive models over arbitrary
lengths and text — attacker-influenced by design, since a boot audit line
carries caller-controlled text into an `ArrayString` and a console ring takes
whatever a keyboard produces — and `tests/fuzz_intrusive.rs` drives two lists
over one shared store against naive order models. Both run under
`cargo xtask fuzz`; `cargo xtask miri` interprets the suite under the
undefined-behaviour oracle, which is what the inline slot arrays need: a test
suite says what the code computes, only an interpreter says whether a raw
pointer stayed in bounds and whether a slot was initialised before it was read.

## Stability

**experimental.** The public API may change until the first tagged release.
