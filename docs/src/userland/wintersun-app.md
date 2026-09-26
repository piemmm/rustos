# The WinterSun client shell

`userland/games/wintersun/app` (`tairix-wintersun-app`) is the window a player
looks through and the frame they see in it: the camera, the tiled software
renderer, the terrain splat and its light composite, the figures standing on
the ground, the fixed-tick pacing the render interpolates over, the input
drain, the three window size states, the degradation ladder a blown frame
budget turns, and the one reference scene every check of the picture draws. It
is `plans/WINTERSUN.md` WS5, WS6 and WS23, and a crate of the
`userland/games/` leaf subtree. Stability tier: **experimental**.

Everything with behaviour is the crate's `[lib]`; the `[[bin]]` is the on-disk
`wintersun.app` bundle's `Run` entry point and only composes it over
`lib/window`'s client half. A freestanding binary is reachable by no host
test, which is how a companion that walked on the spot once survived three
green pipelines.

## No floating point here

The library denies `clippy::float_arithmetic`. Rust's integer arithmetic is
exactly specified on every target and nothing here folds a `usize` into a
result, so the ground a frame draws is bit-identical on `x86_64`, `aarch64`,
`riscv64` and `wasm32` by the language's own rules rather than by luck.

That matters because the frame is where four crates meet. The world generator
and the figure engine work in `f64` and each carries its own cross-target
vertical; the ground art is integer throughout and deliberately carries none.
That the parts agree separately does not say the composition does, so
`digest::REFERENCE_DIGEST` is folded over two whole composited frames with
figures standing in them, and asserted by a vertical on each Tier-1 target.
The art crate's own digest is folded in after the pixels, so a change to the
ground moves this number too — the coverage the art crate does not carry
itself.

## The camera

An orthographic, axis-aligned projection: a scale and a translate, both
integer. A pixel spans a power-of-two number of world sub-units at the
window's own resolution, and a render target drawn smaller spans exactly the
inverse of its fraction more. Every fraction the ladder and the window's cap
use keeps that a whole number of sub-units at every zoom, so the terrain pass
still steps a span by adding the step, and a smaller target covers the same
piece of the world rather than a different one at a coarser resolution. There
are five zoom stops, each a doubling, from one world cell across 128 pixels to
one across 8.

The camera carries the realm's extent and clamps **where the view is
projected**, not where it is aimed. A camera that clamped on being aimed is
correct until the window grows: the wider view it then projects reaches past an
edge it had already settled against, and the frame draws ground the realm does
not have. Clamping at projection makes "the view is inside the realm" true of
every extent rather than of the last one the camera was told about. The
property model found that hole; the fix removed the class of it.

A realm narrower than the view is centred rather than pinned to a corner.

## The frame

A tiled, threaded software renderer over `lib/parallel`, drawing into a
`lib/raster` surface cut into row bands. A band is a full-width run of rows,
because every pass steps *horizontally* — the splat walks a span of pixels
inside one cell row, the composite walks a row of the buffer — so a vertical
cut would divide the unit each is built around. Bands are cut finer than the
runner is wide, so a core taken by another tenant holds up one small piece
rather than a whole share of the frame. A target a clip window cuts is refused
rather than drawn from the wrong column.

The passes run in a fixed order: the terrain splat, the light and fog
composite over it, then the figures. Everything a band needs is resolved
before any band starts: the chunks are already resident, the lattice is
sampled, the material tiles are made resident by the one mutable pass over the
cache, and every figure is posed and placed. The bands then only read. That is
what lets them run on other cores at all, and it is the same discipline that
keeps a frame off the filesystem.

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

The client holds only the ground its view needs and a chunk around it
(`terrain::worth_holding`), giving back the rest as the view moves on, so
however far the player walks what it holds is the view's working set; the
margin keeps a view panning over an edge from giving away ground it is about
to ask for again.

### The light

A single directional light at a shallow angle: long shadows, a cold-to-warm
gradient across every slope, and no light to trace. The shading is the
terrain's own gradient dotted with one vector, and it saturates at the
greatest rise a body may step up — the line the terrain itself draws between a
slope and a cliff face, so ground a player can walk over is shaded across its
whole range. The figures are lit by the same sun, built from the same
direction as the one the art harness measures them under.

Shading is **relative**: a slope facing neither way draws the material's own
colour and the sun brightens or cools it from there. A plain multiply would
darken every surface in the world by whatever the tint's mid-point happened to
be, which is a palette change wearing lighting's clothes.

The result is low-frequency — a function of slope and height, both of which
vary over cells rather than pixels — so it is accumulated into a buffer at a
fraction of the render resolution and upsampled, which is what makes it
affordable and what gives the ladder a knob that costs almost no fidelity for
most of its travel.

### The figures

`figures::Cast` is everyone in the scene, each an actor from the figure engine
standing at a world point. A frame poses and places every figure that can
reach the view — culled by the furthest any figure draws from its ground point
— one to a core, then orders them far to near by ground position, with the
entity id breaking a tie the same way every frame. Each band then draws, in
that order, the figures whose rows reach it, confined to its own rows, so a
figure straddling two bands is two halves that meet exactly.

Figures are drawn **after** the light composite, because the light buffer is
the ground's own relief shading: a figure standing on a slope is not tilted
with it, and it is already shaded from its own surfaces by the same sun. What
a figure does take from the ground is the air: the mist at its feet veils its
every stroke, so a figure in a hollow is as misted as the ground around it.

A figure standing in water is sunk by the depth the rules report at its cell
and nothing of it is drawn below the surface; its contact shadow thins as the
water deepens.

## The command line

`wintersun` takes no operands. `-h`, `-?` and `--help` print the bundle's own
short help, and win wherever they are reached; `--reference-scene` holds the
reference scene still in place of a new world (below); `--` ends the options.
Anything else, read left to right before a help switch, is a usage error with
exit status `2`. A reference scene that cannot be drawn exits `87` with its
reason, since a window holding some other picture would defeat the mode.

## The reference scene

`reference` is one realm, one cast and one moment, drawn by three consumers
that must agree to the bit: the cross-target digest folds it at two fixed
sizes, `Run --reference-scene` holds it in a live window, and the client
vertical draws it on the host at that window's size to check what the guest
scanned out. Everything that decides a pixel is fixed there — the seed, the
cast, the moment, the material cache's size — except the pressure the cache is
taken under, which is the caller's: the digest and the host take it at
normal pressure whatever the machine says, and a live window takes it under
the process's own gauge so it gives memory back like any other cache. A frame
whose cache refused a tile is drawn with that material flat and is not the
reference, so the renderer counts refusals, the digest fails rather than fold
one, and a live window says so on standard error.

The window holding it draws again only when its extent changes or the session
gave its copy of the pixels back, so a still scene costs no frames.

## The player

The player walks as a preset record the bundle ships in its own `Resources/`,
read once before the window opens; where it cannot be read the client says
why and walks as the reference figure instead. The body the rules collide is
as wide as the figure is drawn, so two bodies the simulation lets touch are
drawn touching and never through one another. Each frame the figure is moved
to where the frame shows the body — the same interpolated point the camera
follows — over the real time since it was last posed, and a paused game poses
its figure at the moment it paused.

## Input

Every event already queued is applied before a frame is produced from the
state those events left, so a burst of pointer or key input is one paint
rather than one per event, and a run of resize samples folds to the newest in
the reader.

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

A minimized window stops the clock and draws nothing until the compositor
gives it focus or a size again; the frame on screen when it went is the frame
that comes back. The shell models losing the seat the same way, but no seat
notice reaches a window application yet (`plans/WINTERSUN.md` WS6).

## The degradation ladder

A renderer that sheds whatever is cheapest degrades unpredictably, and a
reviewer cannot tell a deliberate trade from a bug. So the order is fixed and
total, one notch at a time:

1. particle density
2. light-buffer resolution
3. detail-material octaves
4. shadow softness: every shadow edge hardens — each figure's contact shadow
   becomes one ellipse and the relief term measures across one cell rather
   than two — then the relief is dropped. A contact shadow never goes: it is
   what says where a figure stands and whether it has left the ground.
5. render scale, upscaled to the window

Each rung sheds through its own notches before the next is touched, so two
machines at the same step are drawing the same picture and the step is the one
number a diagnostic has to report.

The ladder has a **floor**: the deepest step whose frame still draws the
smallest figure a record describes at the art harness's own readability floor,
for the window's size and the zoom the player chose. Only the render scale
makes a figure smaller, so the floor falls in its rung; where the zoom already
draws figures below that size, the player's choice has made the call and the
render scale does not move. The governor holds the floor before every frame,
so a zoom out or a smaller window takes the ladder back at once, and it never
sheds past it. Overrunning at the floor — or at the ladder's end — is
reported once as it happens: the frame rate is what gives way.

The governor needs three consecutive overrunning frames to shed and sixty
comfortable ones to restore, with the restore threshold well below the shed
one, so a machine that is only just fast enough settles rather than
oscillating.

## The window

Three size states over the window channel: restored, maximised, and exclusive
fullscreen. The client *asks* and the compositor *answers*; nothing assumes a
request took effect. The state rides on the resize event alongside the extent
precisely so the two cannot be believed separately — an app that learnt it was
fullscreen before its extent would lay out edge-to-edge at the old size.

Leaving fullscreen returns to the state it was entered from, so a maximised
window comes back maximised.

The window opens at the baseline resolution at the desktop's scale, capped to
the screen by the one rule every application's window size goes through, so
on a screen smaller than the baseline the window still fits and its title
bar's controls stay reachable. A frame the renderer refuses is withheld, so the
window keeps showing the last frame drawn rather than a half-painted one, and a
run of refusals is reported once.

## The frame budget

The baseline is 1280×720 at 60 Hz — a 16.6 ms frame — on a four-core machine,
with the per-pass allocation `plans/WINTERSUN.md` states. `tests/budget.rs`
measures it rather than asserting about it, on one thread and on four, with the
plan's sixty-four rigged figures standing on the ground, and prints what each
pass cost against its budget and what placing one figure costs a single core.
No elapsed time is asserted: a wall-clock bound is a claim about the machine,
so what the test gates is that real threads draw the picture one thread does.

The material cache is a share of the machine's memory, asked of the System
Information API once before the window opens, and obeys the band the kernel
publishes to the process. A system that will not say how much memory it has
admits no tiles, and the ground is drawn in its materials' flat tones.

The headroom is *derived* from the frame and the named passes rather than
stated beside them: two numbers that must add up are one number and a
subtraction.

## Tests

The host suite covers the projection, the ladder and its floor, the render
target and its bands, the terrain lattice, the light model, the cast and the
figure pass — culling, depth order, the waterline, and bands drawing exactly
what one band draws — the pacing, the input mapping and the size-state model.
`tests/bands.rs` asserts that cutting a peopled frame for any number of
threads produces the identical picture. `tests/proptest_model.rs` is the
invariant model over generated window, input and frame-cost programs — it is
what found the camera's resize hole, and it holds the ladder above its floor.
`tests/budget.rs` is the measurement.

The cross-target rendering claim is four verticals —
`client_frame_qemu_{aarch64,riscv64,x86_64}` and `client_frame_wasm32` — each
folding two composited frames into `digest::REFERENCE_DIGEST`. They are also
what first builds the ground art and the figures for each Tier-1 target.

`wintersun_client_qemu_aarch64` is the end-to-end claim: the installed bundle
is launched by name from a terminal on the reference scene, and its window is
read back as it opened, fullscreen, restored and maximised. Each dump waits on
the session's witness that the window is on screen at that state's extent, and
is compared pixel for pixel with the scene drawn on the host at the extent the
window manager gives it, the cursor and the window's rounded corners aside.
The witness also names how the frame reached the display, which on this
software-composited board must be `composited`.
