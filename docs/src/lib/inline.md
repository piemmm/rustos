# `tairix-inline`

The allocation-free containers TAIRiX runs on, whose storage lives inline in
something the caller already owns.

**Linking this crate never requires a global allocator.** It depends on
nothing — not even `alloc` — and that is the whole reason it is separate from
[`tairix-collections`](./collections.md). The layer that runs before a heap
exists needs containers, and a container crate that links `alloc` forces an
allocator requirement onto every consumer whether or not it ever allocates.
Three of the four architecture ports allocate nothing in production, the boot
console and the early-boot audit ring allocate nothing, and a firmware loader
cannot; they all link this crate, and none of them acquires a heap by doing so.
`plans/COLLECTIONS.md` is the ledger across both container crates.

## Inventory

| Type | Guarantee | Stands in for |
|---|---|---|
| `ArrayVec<T, N>` | up to `N` elements inline, allocating nothing | an ad-hoc `[T; N]` paired with a length field |
| `ArrayString<N>` | up to `N` bytes inline, the UTF-8 invariant held by construction | an ad-hoc `[u8; N]` plus length, with the invariant assumed |
| `RingBuf<T, N>` | a fixed-capacity circular queue, constant time at both ends | every hand-rolled type-ahead buffer, diagnostic tail, and driver hand-off ring |
| `SecretRing<T, N>` | the same, scrubbing each slot it vacates | a ring a credential transits, zeroed by hand at each call site |
| `IntrusiveList` | a doubly-linked list over nodes the caller owns: constant-time unlink of any node, no search | a hand-written `next`/`prev` index pair with its own sentinel and splice arithmetic |
| `BitSet256` | a fixed 256-bit set: constant-time membership | a process's capability membership (`lib/caps`) |

## The rules every container obeys

1. **Nothing allocates, ever.** A capacity is the caller's own bound, chosen at
   the use site; reaching it is answered with `CapacityError` holding the
   refused value, never a panic and never a heap block.
2. **A capacity fault answers with `Result`, an index fault with `Option`, and
   no operation carries both.** That is why the fixed-capacity vectors offer no
   positional insert: its two failure modes — no room, and an index past the
   end — are unrelated, and no single error spells them honestly.
3. **Secret hygiene is the holder's job.** A container does not scrub the slots
   it frees — reuse inside one address space is not a security boundary — so a
   holder of a key, credential, or capability token stores a value type that
   zeroes itself on drop, exactly as the userland heap in `lib/rt` already
   requires. A value that merely *transits* a long-lived kernel buffer is the
   one exception, and it has its own type: see `SecretRing` below.
4. **Usable from interrupt context.** No allocation and no lock means nothing
   here can block or re-enter an allocator, which is what lets an ISR hand off
   through one.

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

## Measurement

Work counters, not elapsed time, are what these are gated on: a counter is
reproducible on any machine where a stopwatch is not. A mid-list unlink reaches
the departing node and its two neighbours and writes exactly those three links,
identically over three nodes and over ten thousand — which is what "constant
time, no search" is, stated as something a regression shows up in. An
`ArrayVec`'s footprint is its elements plus its length and no heap block.

## Tests

Unit tests live next to the code: one drop per element ever taken across a bulk
push, a wrapped drain and an eviction; a `retain` predicate that unwinds
leaving the vector describing exactly what it has written back, so the unswept
tail leaks where a double drop would be unsound; an `ArrayString` that never
stores a partial character however the text was cut; and a `SecretRing` that
leaves nothing but the blank in a slot it has vacated.

`tests/fuzz_inline.rs` drives the tier against naive models over arbitrary
lengths and text — attacker-influenced by design, since a boot audit line
carries caller-controlled text into an `ArrayString` and a console ring takes
whatever a keyboard produces — and `tests/fuzz_intrusive.rs` drives two lists
over one shared store against naive order models, since a free list's
link/unlink order is whatever a process's allocation pattern makes it. Both run
under `cargo xtask fuzz`, and `cargo xtask miri` interprets the suite under the
undefined-behaviour oracle — the inline slot arrays are why that stage exists.
A test suite says what the code computes; only an interpreter says whether a
raw pointer stayed in bounds, whether a slot was initialised before it was
read, and whether two `&mut` ever aliased.

## Stability

**experimental.** The public API may change until the first tagged release.
