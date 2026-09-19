# tairix-wintersun-rules

WinterSun's authoritative simulation: the fixed-rate tick, and every rule it
applies (`plans/WINTERSUN.md` §5). Stability tier: **experimental** —
nothing has shipped, so a curve or a step order may still change in place.

`no_std` (needs `alloc`), `forbid(unsafe_code)`.

## What this crate is for

A client sends *intents* and is told what happened. It never asserts a
position, a hit, or a balance: those are computed here, from inputs this
crate validated. That is what makes a realm authoritative rather than merely
well-behaved, and it is why the same code runs on the realm server and —
for its own prediction — on the client.

## The tick

One step is **total**: every phase runs, every tick, and the order is fixed
and documented on `Zone::step`. Two realms resolving a tick in a different
order would diverge, and nothing downstream recovers from that.

| Phase | What it does |
|---|---|
| 1 | Clear the previous step's notifications and refusals. |
| 2 | Bucket every body in the broad phase, from the positions the previous step left. |
| 3 | Statuses: periodic effects, then durations and the diminishing ledger age. Before movement, so a root landing this tick binds this tick. |
| 4 | Intents, in the order the clock placed them — earliest sample first, then by identity. |
| 5 | Movement, in identity order. |
| 6 | Separation: gather every overlap's correction, then apply them all at once. |
| 7 | Reap the dead, one notification each. |
| 8 | Advance the tick and refill each body's intent budget. |

Bodies are iterated in identity order everywhere, out of an array kept sorted
by an identity the zone mints monotonically.

## Determinism

The simulation is bit-identical on `x86_64`, `aarch64`, `riscv64` and
`wasm32`, and that is a test rather than an intention: every target plays one
scripted session and asserts `digest::REFERENCE_DIGEST`.

It is cheap to hold because almost nothing here is floating point. Movement
is fixed-point integer arithmetic with a carried remainder; separation uses
an exact integer square root; every curve, mitigation and duration is integer
arithmetic over a bounded domain. The single exception is turning a held
direction into a heading, which goes through `lib/util::mathf` — TAIRiX's own
libm — so the same source yields the same bits everywhere.

The four-target vertical still runs, because "should be identical" and "is
identical" are different claims. `wasm32` in particular is the only Tier-1
target with a 32-bit `usize`.

## What a client cannot lie about

The realm's tick counter is the only clock.

| Claim | Answer |
|---|---|
| "More time passed than you think" | Nothing reads a client duration. Cooldowns, durations and regeneration are counted in ticks the realm counted. |
| "I acted in a tick that has not happened" | Refused. |
| "I sampled this long ago, so order it first" | Honoured inside a bounded window, clamped to its edge beyond — so back-dating further buys no further priority. |
| "I moved this far" | Not a number the wire carries. Distance is computed from the mover's own speed and resolved against the ground. |
| "Apply this intent again" | Each sequence is adjudicated once. |

## Modules

| Module | What it owns |
|---|---|
| `bounds` | The fixed ranges every rule is defined over. Not capacities. |
| `clock` | The tick rate, and the validation and clamping of a client's sample time. |
| `stat` | The closed five-stat set and the curves everything derives from. |
| `pool` | Health and resource: a value inside `0..=max`, with no path out of it. |
| `status` | The eleven-kind vocabulary, its stacking rules and its diminishing returns. |
| `damage` | The five ordered steps of the damage pipeline, and healing beside it. |
| `terrain` | The collision field: the ground seam, the passability rules, and the chunk-backed and pattern implementations. |
| `space` | The broad phase: one sorted array, rebuilt per step. |
| `entity` | One body, its numbers, and its carried remainder. |
| `motion` | Fixed-point movement, wall sliding, and integer separation. |
| `zone` | Every body in a region, and the step that advances them. |
| `digest` | The state hash a desync is bisected with, and the scripted run the cross-target claim is staked on. |

## Bounds are not capacities

Nothing here caps how many bodies a zone holds, how many notifications a step
emits, or how many cells the broad phase occupies: those follow the work and
grow on demand, failing closed as a typed error only on genuine exhaustion.
What is fixed is the range each *rule* is defined over — how much authority
one intent carries, how far a status reaches, how fast anything can be.
Those are bounds on untrusted input and on arithmetic range, and they do not
move to admit a value.

The arithmetic argument matters as much as the security one: every product
the pipelines form is bounded by them, which is what lets the whole
simulation run in checked integer arithmetic with no saturating step hiding a
real overflow.

## What this crate does not do yet

Movement is the whole of what it resolves. There is no action table, no spell
book, no inventory and no interaction, so an intent naming one is refused as
unresolvable — which is the same refusal the lookup will give once a table
exists. Combat actions arrive with WS9, magic with WS10, progression and
items with WS11; the damage, status and resource verbs they will call are
here and tested.

## Tests

`cargo test -p tairix-wintersun-rules` covers every rule with hand-computed
cases. `tests/proptest_model.rs` drives generated programs of legal actions
against a live zone and checks every bound after each command; it is enrolled
in `cargo xtask proptest`. The cross-target claim is
`tests/integration/rules_determinism_qemu_{aarch64,riscv64,x86_64}` and
`tests/integration/rules_determinism_wasm32`.
