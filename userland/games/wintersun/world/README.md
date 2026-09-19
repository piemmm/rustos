# tairix-wintersun-world

WinterSun's seed-pure procedural world: a `u64` seed, eight validated
parameters, and every chunk of ground that follows from them
(`plans/WINTERSUN.md` §2). Stability tier: **experimental** — nothing has
shipped, so a stage's arithmetic may still change in place.

`no_std` (needs `alloc`), `forbid(unsafe_code)`.

## Why the world is generated and not sent

A realm's terrain is public — a player can see the hills by walking to them —
so transmitting it would be paying bandwidth for something the seed already
says. The client and the realm server generate it independently and get the
same answer. What the seed *cannot* predict — entities, and the changes
players made — is the realm's to send; what the realm keeps secret (a
dungeon's interior, an unopened container, an undetected trap) has no
encoding here at all, which is a stronger guarantee than not sending it.

That only works if the two agree exactly. A one-sub-unit disagreement is a
player walking through a hill on one machine and around it on another, and no
netcode recovers from that.

## Two scales

Some questions are not local. Discharge depends on the whole upstream basin;
a rain shadow records everything the wind crossed to get there; a road is
only a road if it reaches the town at its far end. A generator that answers
those from a window around one chunk produces rivers that flow uphill across
the seam between two windows.

| Scale | What it is | Cost |
|---|---|---|
| `realm` | The whole world, solved once, coarsely: plates, relief, depression-filled drainage, stream-power erosion, lakes, climate, settlements, roads, landmarks. **Global and exact**, so everything derived from it is seam-free by construction. | A fixed sample count, never a step in world units — so a realm four chunks across and one four thousand chunks across pay the same. |
| `chunk` | Fine detail on demand: detail relief, channel carve, structure stamp, climate correction, biome classification, scatter. Reads the realm field plus a fixed ring of cells. | Per chunk, cached and reclaimed. |

## Modules

| Module | What it owns |
|---|---|
| `params` | The realm parameter document and its validating constructor. Every field is bounded: a client is handed these by its realm, and a realm is no more trusted than a client. |
| `seed` | Domain-separated randomness — a keyed hash over coordinates, not a sequence, so no value depends on traversal order. |
| `noise` | Value noise, fBm, ridged, billow, domain warp. |
| `uplift` | Continental plates as a jittered-grid Voronoi, their drift, and the uplift their boundaries produce. |
| `relief` | Coarse heightfield, and the sea-level cut that honours the requested submerged fraction. |
| `hydrology` | Priority-Flood depression filling, D8 routing, accumulation, stream-power incision and hillslope diffusion. |
| `climate` | Temperature (latitude, lapse rate, continentality) and moisture advected along the prevailing wind, with orographic lift and rain shadow. |
| `sites` | Settlement placement, a minimum spanning tree over them, and integer-cost A\* road routing that reuses existing road — so roads braid. Landmark entrances. |
| `biome` | The material set and the Whittaker classification that blends it into a normalised weight vector. |
| `scatter` | Vegetation, rock and resource nodes, by jittered grid with a priority rule rather than dart-throwing. |
| `chunk` | The chunk value and the resumable phase machine that builds it. |
| `cache` | The `lib/reclaim`-governed chunk cache. |
| `digest` | The cross-architecture determinism digest and its reference constant. |

## What "seed-pure" means, exactly

Every answer is a pure function of `(seed, parameters, position)` — not of how
many chunks have been asked for, nor in what order, nor on which machine:

- **Randomness is random-access.** A keyed hash over coordinates
  (`lib/hash`'s XXH64, whose writes are little-endian on every port), never a
  sequence a traversal could advance differently. `seed::Stream` exists only
  where a stage genuinely wants several correlated draws about one thing, and
  is deliberately not `Clone`.
- **Arithmetic is IEEE-754 `f64` restricted to the exactly-specified
  operations plus `lib/util::mathf`**, TAIRiX's own libm. A platform libm
  would compute a transcendental differently per target; there is none here,
  and no `mul_add`, so `a * b + c` stays two operations.
- **Everything stored is a quantised integer on a power-of-two scale**, so
  the conversion is exact and a stored value is a bit pattern rather than a
  rounding.
- **Every sort, priority queue and traversal has a total order with an index
  tiebreak.** An `f64` comparator has no total order, so the flood's heap, the
  wind sweep and the road search all order integers.

The claim is staked on `digest::REFERENCE_DIGEST`: one constant that the host
suite and one vertical per Tier-1 target each assert
(`tests/integration/world_determinism_qemu_{aarch64,riscv64,x86_64}` and
`tests/integration/world_determinism_wasm32`). Agreement between targets
follows from each agreeing with the constant, so a target that has never run
cannot pass by accident. `wasm32` is the leg most likely to catch a real
divergence: it is the only Tier-1 target with a 32-bit `usize`.

## Memory

Resident cost is the working set, never the world's extent. The realm field is
a fixed grid whose size does not follow the realm's size; chunks live in a
`lib/reclaim` cache budgeted from the memory the machine reported and released
through the system's pressure bands. A generated chunk is never written to
disk — recomputing it is cheaper than reading it back, and only the changes
players make are worth storing, which is what makes a realm O(changes) on disk
and O(working set) in RAM whatever its extent.

Allocation is fallible throughout: a machine too short to hold the field gets
a typed `WorldError::OutOfMemory`, never an abort.

## What is not here

- **No byte decoding.** The wire encoding of a parameter document belongs with
  the rest of the protocol in `wintersun/net`, which already owns bounded
  decode and its fuzz harnesses. A second decoder here would be a second place
  to get it wrong, so this crate has no untrusted-input parser and therefore
  no fuzz target of its own.
- **No secrets.** Terrain is public by design. A dungeon's interior, an
  unopened container's contents and an undetected trap's position are the
  realm's, generated server-side and streamed under interest management; a
  design that let a client derive one from the seed would be a defect.
- **No storage.** Player-caused change is the realm store's business
  (`plans/WINTERSUN.md` WS7).
