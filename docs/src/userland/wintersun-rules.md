# The WinterSun simulation

`userland/games/wintersun/rules` (`tairix-wintersun-rules`) is the
authoritative simulation: the fixed-rate tick, and every rule it applies. It
is `plans/WINTERSUN.md` WS3, and the third crate of the `userland/games/`
leaf subtree. Stability tier: **experimental**.

## Authority, and what follows from it

A client sends *intents* — "I am holding north-west", "cast spell seven at
this point" — and is told what happened. It never sends state. Position,
health and every balance are computed here, from inputs this crate
validated, so there is no trust level a client can reach that shortens the
path.

The practical consequence is that the interesting questions are not "did the
client cheat?" but "what is there for it to assert?". Distance moved is not a
number the wire carries at all: the realm takes the direction a body is
holding, multiplies it by the speed that body's own stats and statuses
allow, and resolves the result against the ground. A client that wanted to
move faster would have to change its stats, which are the realm's.

## The tick is fixed-rate and total

The step runs at a realm parameter, thirty hertz by default. Thirty rather
than twenty because this is an action game with dodges and aimed shots, and
a twenty-hertz step quantises every input to fifty milliseconds — enough to
make a dodge feel unreliable in a way no client-side polish hides.

One step is *total*: every phase runs, every tick, with no work deferred to
"whenever the next one happens". Its order is fixed and documented, because
two realms resolving a tick differently would diverge and nothing downstream
recovers from that.

| Phase | What it does | Why there |
|---|---|---|
| 1 | Clear the previous step's notifications and refusals. | Each step reports its own tick. |
| 2 | Bucket every body in the broad phase. | From the positions the previous step left final. |
| 3 | Statuses: periodic effects, then ageing. | Before movement, so a root landing this tick binds this tick. |
| 4 | Apply admitted intents. | In the order the clock placed them: earliest sample first, then by identity. |
| 5 | Movement. | In identity order. |
| 6 | Separation. | Gathered, then applied at once, so no body's outcome depends on whether its neighbour went first. |
| 7 | Reap the dead. | One notification each. |
| 8 | Advance the tick, refill each body's intent budget. | |

"Whatever order the map iterated" is a defect, not a detail, so bodies are
iterated out of an array kept sorted by an identity the zone mints
monotonically.

## The clock cannot be lied to

The realm's tick counter is the only clock. Every cooldown, duration and
regeneration is measured in ticks the realm counted, which closes the whole
speedhack family at the source rather than by detecting it afterwards.

What a client *may* say is where inside a tick it sampled an input, and that
is worth accepting: a fixed step otherwise quantises every action to its
boundary. So the sample time is an ordering key, not a duration, and it is
validated before anything reads it:

- a sample ahead of the realm is refused — acting in a tick that has not
  happened is the one claim no generosity covers;
- a sample older than the lookbehind window is clamped to the window's oldest
  edge, so back-dating further buys no further priority;
- anything inside the window is taken as sent, which is the point of
  accepting it.

Each intent's sequence is adjudicated once, and a body has a bounded number
of intents per tick. A refusal is an answer the client is told, never a
silent drop: an action that vanishes reads to a player as a bug.

## Determinism, and why it is nearly free here

The simulation is bit-identical on all four Tier-1 targets, and that is a
test rather than an intention. Every target plays one scripted session —
twelve bodies of differing stats walking obstructed ground, colliding,
striking, healing, carrying statuses that stack and diminish, one of them
dying — and folds the *whole trajectory* into one number that must equal
`digest::REFERENCE_DIGEST`.

Holding that claim is cheap because almost nothing in the crate is floating
point:

- movement is a fixed-point multiply, a floor and a carried remainder;
- separation uses an exact integer square root, never a float distance;
- every curve, mitigation and duration is integer arithmetic over a bounded
  domain;
- anything folded into the digest has a fixed width, never `usize` — whose
  four bytes on `wasm32` would otherwise put the host's pointer size into the
  answer.

The single exception is turning a held direction into a heading, which goes
through `lib/util::mathf`, TAIRiX's own libm, so the same source yields the
same bits on every target.

The world generator's cross-target claim is a separate vertical with a
separate constant. The session here walks a *pattern* rather than a
generated realm, so a change to either cannot make the other's evidence
ambiguous.

## The carried remainder

Speed is sub-units per tick and a held direction is a fraction of it, so a
step is a division. A division that truncated every tick would quietly make
diagonal movement slower than cardinal, and a slow walk slower still, in a
way a player feels and cannot name. Each body therefore carries the
fractional part of its own step forward. It costs two words and nothing in
determinism, because it is integer bookkeeping.

## The collision field

The simulation needs two numbers per cell — how high the ground is and how
high the water over it stands — and nothing else. The seam is therefore a
data source rather than a policy: an implementation answers those two, and
the rules that read them live in the crate, once.

That split matters. *Fetching* generated chunks is a cache with a memory
budget and a pressure policy, which belongs to the process holding it.
*Interpreting* them is a rule the realm and every client must agree on
exactly, so it is in the crate and host-tested.

Ground the caller does not have reads as impassable, so a body cannot walk
off the edge of what the realm has generated. Deep water is impassable and
the shallows are not, which is also what makes open sea impassable without a
separate flag for it. A cell raised more than a step above its neighbours is
a cliff, which is how the field expresses an obstacle without a second
vocabulary for one.

A body is a box against the ground and a circle against other bodies:
conservative against terrain, so it cannot clip a corner into a wall; exact
against bodies, because two players standing together is something players
look at.

## The broad phase

A crowd is the load case every persistent world meets on its first busy
evening, and an all-pairs scan is quadratic in exactly that case. Bodies are
bucketed by cell, so a query costs the cells it overlaps rather than the
zone's population.

The structure is one array of `(cell, body)` sorted row-major and rebuilt in
place each step — not a map of cells to lists. A hash map's iteration order
varies with its keys and its insertion history, and an authoritative
simulation cannot read an order that does; a map of lists is also an
allocation per occupied cell, churned every tick. Sorting groups a row
contiguously, so a rectangle query is one binary search per row and a walk,
the order is total with the body's identity as tiebreak, and the allocation
is reused for the process's life.

The query is conservative: it takes the radius it wants and visits every cell
the circle's bounding box touches, so correctness does not depend on the cell
size — only cost does.

## Stats, damage and status

Five stats, closed, with every derived quantity a stated integer function of
them. Resistance is `stat / (stat + half)`, which approaches total and never
arrives, so no stat value makes a target immune — a property of the curve's
shape rather than a clamp bolted on top.

Damage is five ordered steps, each a pure function with its own test: the
attacker's power scales the authored blow, armour subtracts and resistance
scales, the defender's statuses modulate, a floor keeps a landed blow from
doing nothing, and shields absorb before health does. The order is the whole
specification — the same numbers in a different order are a different game.
The floor is what makes the flat armour term safe: a hit that connects and
does nothing is indistinguishable from a miss, which reads as a bug rather
than as defence.

Eleven status kinds, closed, with three rules that keep the set from being
exploitable:

- **proportional effects take the strongest, never the sum** — two slows that
  added would exceed a full stop, and two mitigations that added would reach
  total immunity; resource-shaped effects (a bleed, an absorb pool) do sum,
  because summing is what they mean;
- **the set is partitioned into harmful and helpful, each with its own
  ceiling** — one shared ceiling would let a player fill it with self-applied
  buffs and become unstunnable;
- **losing control diminishes; taking damage does not** — each application of
  a stun, root, silence or slow within the window is worth half the last and
  the fourth is refused, because repeated loss of control is what makes a
  game unplayable rather than hard.

A root or a stun works through the speed rather than by refusing the input.
Refusing would leave a stale held direction to resume the moment it expired,
so a body that changed its mind while held would walk the old way.

## Bounds are not capacities

Nothing here caps how many bodies a zone holds, how many notifications a step
emits, or how many cells the broad phase occupies: those follow the work and
grow on demand, failing closed as a typed error only on genuine exhaustion.
What is fixed is the range each *rule* is defined over.

Those fixed ranges do a second job. Every product the pipelines form is
bounded by them, which is what lets the whole simulation run in checked
integer arithmetic — the workspace builds with overflow checks on in every
profile — with no saturating step hiding a real overflow.

## What is not here yet

Movement is the whole of what the crate resolves. There is no action table,
no spell book, no inventory and no interaction, so an intent naming one is
refused as unresolvable — the same refusal the lookup will give once a table
exists. Combat arrives with WS9, magic with WS10, progression and items with
WS11. The damage, healing, status and resource verbs those will call are
here and tested.

## Verification

Every rule carries hand-computed cases beside it. `tests/proptest_model.rs`
drives generated programs of legal actions against a live zone and checks
every bound after each command; it is enrolled in `cargo xtask proptest`. The
cross-target claim is four verticals —
`tests/integration/rules_determinism_qemu_{aarch64,riscv64,x86_64}` under
QEMU and `tests/integration/rules_determinism_wasm32` under a WebAssembly
engine.
