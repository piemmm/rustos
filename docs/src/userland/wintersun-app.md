# The WinterSun client shell

`userland/games/wintersun/app` (`tairix-wintersun-app`) is the window a player
looks through and the frame they see in it: the camera, the tiled software
renderer, the terrain splat and its light composite, the fixed-tick pacing the
render interpolates over, the input drain, the three window size states, and
the degradation ladder a blown frame budget turns. It is
`plans/WINTERSUN.md` WS5, and the fifth crate of the `userland/games/` leaf
subtree. Stability tier: **experimental**.

Everything with behaviour is the crate's `[lib]`; the `[[bin]]` is the on-disk
`wintersun.app` bundle's `Run` entry point and only composes it over
`lib/window`'s client half. A freestanding binary is reachable by no host
test, which is how a companion that walked on the spot once survived three
green pipelines.

## No floating point

The library denies `clippy::float_arithmetic`. Rust's integer arithmetic is
exactly specified on every target and nothing here folds a `usize` into a
result, so a frame is bit-identical on `x86_64`, `aarch64`, `riscv64` and
`wasm32` by the language's own rules rather than by luck.

That matters because the frame is where three crates meet. The world
generator works in `f64` and carries its own cross-target vertical; the ground
art is integer throughout and deliberately carries none, because bit-identity
follows from the language there. That the parts agree separately does not say
the composition does, so `digest::REFERENCE_DIGEST` is folded over two whole
composited frames and asserted by a vertical on each Tier-1 target. The art
crate's own digest is folded in after the pixels, so a change to the ground
moves this number too — the coverage the art crate does not carry itself.

## The camera

An orthographic, axis-aligned projection: a scale and a translate, both
integer. A pixel spans a power-of-two number of world sub-units, which is what
lets the terrain pass step a span by shifting. There are five zoom stops, each
a doubling, from one world cell across 128 pixels to one across 8.

The camera carries the realm's extent and clamps **where the view is
projected**, not where it is aimed. A camera that clamped on being aimed is
correct until the window grows: the wider view it then projects reaches past an
edge it had already settled against, and the frame draws ground the realm does
not have. Clamping at projection makes "the view is inside the realm" true of
every extent rather than of the last one the camera was told about. The
property model found that hole; the fix removed the class of it.

A realm narrower than the view is centred rather than pinned to a corner.

## The frame

A tiled, threaded software renderer over `lib/parallel`. A tile here is a
full-width run of rows, because every pass steps *horizontally* — the splat
walks a span of pixels inside one cell row, the composite walks a row of the
buffer — so a vertical cut would divide the unit each is built around. Bands
are cut finer than the runner is wide, so a core taken by another tenant holds
up one small piece rather than a whole share of the frame.

Everything a band needs is resolved before any band starts: the chunks are
already resident, the lattice is sampled, and the material tiles are made
resident by the one mutable pass over the cache. The bands then only read.
That is what lets them run on other cores at all, and it is the same
discipline that keeps a frame off the filesystem.

### The ground

The world generator answers per *cell*; a frame is per *pixel*, and at the
authored zoom a cell is thirty-two of them. So the pass works on a lattice of
the visible cells' weight fields, interpolates it vertically once per raster
row, and hands each horizontal run between two lattice columns to the art
crate's splat, which steps the rest. Four hash evaluations per span rather
than four per pixel.

Roads are stamped into that lattice as decals, converted from the generator's
cell paths once per realm rather than once per frame.

A lattice point whose chunk is not resident is *marked*, not fetched and not
waited for, and the pass draws it as ground the client cannot vouch for —
deliberately not a terrain colour, because a plausible placeholder would have
the client asserting a world it has not been told about.

### The light

A single directional light at a shallow angle: long shadows, a cold-to-warm
gradient across every slope, and no light to trace. The shading is the
terrain's own gradient dotted with one vector, and it saturates at the
greatest rise a body may step up — the line the terrain itself draws between a
slope and a cliff face, so ground a player can walk over is shaded across its
whole range.

Shading is **relative**: a slope facing neither way draws the material's own
colour and the sun brightens or cools it from there. A plain multiply would
darken every surface in the world by whatever the tint's mid-point happened to
be, which is a palette change wearing lighting's clothes.

The result is low-frequency — a function of slope and height, both of which
vary over cells rather than pixels — so it is accumulated into a buffer at a
fraction of the render resolution and upsampled, which is what makes it
affordable and what gives the ladder a knob that costs almost no fidelity for
most of its travel. The point lights a later item adds accumulate into this
same buffer rather than a second one.

## Pacing

The simulation steps at a fixed rate and the display refreshes at whatever
rate it has; tying one to the other would make the sim rate a visual property.
The pacer converts elapsed real time into whole ticks, and what is left over
is the fraction a frame reads *between* the last two authoritative states.

The accumulator counts in nanosecond-ticks rather than nanoseconds divided by
a tick, so a rate that does not divide a second evenly — thirty does not —
loses nothing over an hour. Time beyond a quarter of a second is dropped
rather than replayed, so a client descheduled for a second resumes instead of
spending the next second simulating the last one.

Losing the seat stops the clock and drops focus; the frame on screen when it
went is the frame that comes back.

## The degradation ladder

A renderer that sheds whatever is cheapest degrades unpredictably, and a
reviewer cannot tell a deliberate trade from a bug. So the order is fixed and
total, one notch at a time:

1. particle density
2. light-buffer resolution
3. detail-material octaves
4. shadow softness
5. render scale, upscaled to the window

Each rung sheds through its own notches before the next is touched, so two
machines at the same step are drawing the same picture and the step is the one
number a diagnostic has to report. Frame rate is never what gives way: when
the ladder is spent the governor leaves the renderer where it is.

The governor needs three consecutive overrunning frames to shed and sixty
comfortable ones to restore, with the restore threshold well below the shed
one, so a machine that is only just fast enough settles rather than
oscillating.

Rung 4 turns the *relief-shading stencil* today — a soft term measures across
two cells, a hard one across one, and off drops the term — because the cast
shadows it will also govern arrive with the figures in WS6. It is the wider
stencil that is the penumbra and also the more expensive, which is why the
ladder narrows it before giving the term up entirely.

## The window

Three size states over the window channel: restored, maximised, and exclusive
fullscreen. The client *asks* and the compositor *answers*; nothing assumes a
request took effect. The state rides on the resize event alongside the extent
precisely so the two cannot be believed separately — an app that learnt it was
fullscreen before its extent would lay out edge-to-edge at the old size.

Leaving fullscreen returns to the state it was entered from, so a maximised
window comes back maximised.

## The frame budget

The baseline is 1280×720 at 60 Hz — a 16.6 ms frame — on a four-core machine,
with the per-pass allocation `plans/WINTERSUN.md` states. `tests/budget.rs`
measures it rather than asserting about it, on one thread and on four, and
prints what each pass cost against its budget.

The headroom is *derived* from the frame and the named passes rather than
stated beside them: two numbers that must add up are one number and a
subtraction.

## Tests

The host suite covers the projection, the ladder, the render target and its
bands, the terrain lattice, the light model, the pacing, the input mapping and
the size-state model. `tests/bands.rs` asserts that cutting a frame for any
number of threads produces the identical picture. `tests/proptest_model.rs` is
the invariant model over generated window and input programs — it is what
found the camera's resize hole. `tests/budget.rs` is the measurement.

The cross-target rendering claim is four verticals —
`client_frame_qemu_{aarch64,riscv64,x86_64}` and `client_frame_wasm32` — each
folding two composited frames into `digest::REFERENCE_DIGEST`. They are also
what first builds the ground art for each Tier-1 target.
