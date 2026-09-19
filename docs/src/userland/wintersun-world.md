# The WinterSun world generator

`userland/games/wintersun/world` (`tairix-wintersun-world`) answers what the
ground is at any point of a WinterSun realm, from the realm's `u64` seed and
a small validated parameter document and nothing else. It is
`plans/WINTERSUN.md` WS2, and the second crate of the `userland/games/` leaf
subtree. Stability tier: **experimental**.

## Why the world is generated rather than transmitted

Terrain is public: a player can see the hills by walking to them, so sending
them would be paying bandwidth for something the seed already says. The
client and the realm server generate it independently and get the same
answer. The realm sends only what the seed cannot predict — entities, and the
deltas players caused — which is what makes a realm affordable in bandwidth
and in memory at any extent.

That only works if the two agree *exactly*. A one-sub-unit disagreement is a
player walking through a hill on one machine and around it on another, and no
amount of reconciliation recovers from it. Cross-architecture determinism is
therefore not a nicety here; it is the load-bearing property, and it is
tested as one.

Anything whose value *is* its concealment — a dungeon's interior layout, an
unopened container's contents, an undetected trap — is generated server-side
and streamed under interest management. It has no encoding in this crate at
all, which is a stronger guarantee than not sending it.

## Two scales, and why there have to be two

Hydrology, climate and road routing are not local questions. How much water
passes a point depends on every point that drains to it, which can be most of
a continent. A rain shadow is a record of everything the wind crossed before
it arrived. A road is only a road if it reaches the town at its far end.

A generator that answers those from a window centred on whichever chunk asked
produces a different answer per query — rivers that flow uphill across the
seam between two windows, and a road that stops at the edge of the window
that routed it. So the world is solved on two scales.

### The realm field

The whole world, solved once, coarsely and **globally**. Its stages run in
dependency order, each reading the one before it:

1. **Uplift.** Continental plates as a Voronoi partition over a jittered
   grid, each with a drift and a buoyancy. Convergence raises a belt along
   the seam two plates share; divergence opens a rift. This is what stops a
   heightfield looking like noise — a range gets a direction and a reason.
2. **Relief.** Domain-warped continental noise, plus the uplift field, plus
   ridged noise admitted only in proportion to belt strength, so ridges
   appear along belts and nowhere else. Sea level is then *cut* rather than
   chosen: the raw relief is sorted and cut at the requested quantile, so a
   realm's coastline is the submerged fraction the operator asked for.
3. **Hydrology.** Priority-Flood fills depressions from the outlets inward.
   Its pop order is itself a downstream-first ordering, which gives the
   filled surface (so lakes have a surface and an outflow), a
   guaranteed-acyclic routing, and the traversal order accumulation needs —
   all without an epsilon nudge or an iteration to convergence. Then bounded
   passes of stream-power incision (`E = K·√A·S`) with hillslope diffusion,
   which cut the valleys roads later follow and lay the flats settlements
   later use.
4. **Climate.** Temperature from latitude, an environmental lapse rate, and
   continentality. Moisture advected in one sweep along the prevailing wind,
   ordered by each cell's projection onto it, losing much more where the air
   is forced to rise — so the windward slope is wet and the lee is dry.
5. **Sites and roads.** Settlements on level, watered, defensible ground, kept
   apart. A minimum spanning tree over them, each edge routed by integer-cost
   A\* over a traversal cost field that prefers level ground, pays heavily to
   cross water, and **pays less to reuse a road already routed** — which is
   what makes a network braid and converge rather than run as parallel lines.
   Then landmark entrances.

Its resolution is a fixed sample count, never a step in world units. A realm
four chunks across and one four thousand chunks across get the same grid; the
larger simply has a coarser step. So the field's cost is the same on every
machine and for every realm, and nothing in it grows with the world's extent.

### The chunk

Fine detail, on demand. It reads the realm field, adds everything below the
coarse step, and depends on nothing outside a fixed ring of cells:

1. **Relief** — detail noise in *absolute* world units (texture does not
   scale with the realm the way structure does), ridged inside belts and
   billowed into dunes where it is arid and flat.
2. **Water** — channels carved along the coarse drainage, widening with
   discharge; lakes and sea filled from the coarse water surface; the shore
   distance measured.
3. **Structures** — settlements levelled and roads laid.
4. **Climate** — the coarse values corrected for the fine relief's departure
   from the coarse one.
5. **Biome** — a Whittaker classification into a normalised weight vector.
6. **Scatter** — vegetation, rock and resource nodes.

Scatter is last because it reads the structure stamp: nothing grows on a road.

The build is a resumable phase machine, and the partially-built chunk is
readable throughout, so a client draws the coarse ground it already has rather
than blocking a frame on generation.

## The halo, and why it is exactly as wide as it is

Everything a chunk computes is either a read of the realm field — global, so
identical for every query — or a pure function of absolute position. Neither
depends on a neighbour. One quantity does: the **shore distance**, which is a
distance transform.

So the build works over the chunk *plus a ring* of `SHORE_CELLS`, runs the
transform over the whole working grid, and keeps only the chunk. A cell inside
the chunk therefore gets the same answer it would get if the whole world had
been transformed at once, and "a chunk generated alone equals the same chunk
generated as part of its neighbourhood" is a consequence of the construction
rather than a hope about the width.

Scatter obeys the same discipline by a different route: every scatter cell
offers exactly one candidate at a hashed position with a hashed priority, and
a candidate survives only if nothing within its exclusion radius outranks it.
Exclusion radii are bounded by the scatter step, so the outcome depends on
the eight neighbouring scatter cells and nothing further. Two chunks generated
a week apart agree on every item along their seam because neither ever looked
at the other.

## Biomes are a gradient, not a label

A cell does not have *a* biome. It has a weight vector over the material set,
kept to the four heaviest and normalised to sum to exactly 255 — so "the
weights are normalised" is a property of the type rather than a convention a
consumer has to trust. A boundary between boreal forest and fell heath is
therefore a gradient the splat renderer draws as one.

Memberships use a compactly-supported quadratic kernel rather than a Gaussian:
an exponential is not available in `no_std`, a platform one would not be
identical per target, and a material's influence genuinely *ending* is more
useful than its merely becoming small.

WinterSun's set is cold-biased, as its name promises: glacier, snowfield,
tundra, fell heath, cold steppe, boreal forest, temperate forest, moor,
saltmarsh, ashland, and the rift-scarred waste where the world was torn, plus
water, rock, gravel and sand for the slope and shore overrides.

## Determinism, and how it is proven

The arithmetic is written to be target-independent:

- Randomness is a **keyed hash over coordinates**, not a sequence, so no
  value depends on the order anything was traversed in.
- Arithmetic is IEEE-754 `f64` restricted to the exactly-specified operations
  plus `lib/util::mathf`, TAIRiX's own libm. Rust contracts no fused
  multiply-add, so `a * b + c` stays two operations.
- Everything stored is a quantised integer on a power-of-two scale, so the
  conversion is exact.
- Every sort, priority queue and traversal has a total order with an index
  tiebreak. An `f64` comparator has none, so the flood's heap, the wind sweep
  and the road search all order integers.

"Written to be" is not evidence, so the claim is staked on one constant,
`digest::REFERENCE_DIGEST`: a fixed realm is solved, its coarse field and six
spread-out chunks are folded into a digest, and that digest must equal the
constant. The host suite asserts it, and so does one vertical per Tier-1
target — `tests/integration/world_determinism_qemu_{aarch64,riscv64,x86_64}`
under QEMU, and `tests/integration/world_determinism_wasm32` under a real
WebAssembly engine. Agreement between targets follows from each agreeing with
the constant, so a target that has never been run cannot pass by accident.

The `wasm32` leg needs no browser, unlike the other wasm32 verticals: its
subject is arithmetic, and requiring a headless Chrome would make the fourth
leg runnable in fewer places than the claim it defends applies to. It is also
the leg most likely to catch a real divergence, being the only Tier-1 target
with a 32-bit `usize`.

## Memory

Resident cost is the working set, never the world's extent:

- The realm field is a fixed grid, a few mebibytes at the resolution ceiling
  and the same on every machine.
- Chunks live in a `lib/reclaim` cache whose budget is derived from the memory
  the machine reported — supplied by the caller from the System Information
  API, because a library picking a capacity for itself would be the
  hand-chosen ceiling the charter forbids — and released through the system's
  pressure bands. Its generation token is the realm parameter document, so
  changing any of it invalidates every entry.
- Nothing is written to disk. Recomputing a chunk is cheaper than reading it
  back, so only player-caused change is stored, and a realm is O(changes) on
  disk and O(working set) in RAM whatever its extent.

Allocation is fallible throughout: a machine too short to hold the field gets
a typed refusal, never an abort.

## Bounds

Every field of the parameter document is bounded, because a client is handed
its realm's parameters *by the realm*, and a realm is no more trusted by a
client than a client is by a realm. The type has one validating constructor
and no public fields: an out-of-range extent, a resolution that would not
tile, or a temperature that would not quantise is refused with a reason rather
than clamped into something the two ends might disagree about.

This crate decodes no bytes. The wire encoding of those fields belongs with
the rest of the protocol in `wintersun/net`, which already owns bounded decode
and its fuzz harnesses, so there is no untrusted-input parser here and no fuzz
target of its own.
