# The WinterSun figure engine

`userland/games/wintersun/figure` (`tairix-wintersun-figure`) answers what a
character *is* before anything animates it: a skeleton of joints, the
parametric parts bound to them, the sockets equipment hangs on, and the one
projection that turns all of it toward the camera — the validated record a
character is built from, and the designer that edits one. It is
`plans/FIGURE.md` FG2–FG7, and the sixth crate of the `userland/games/` leaf
subtree. Stability tier: **experimental**.

## Parts on a skeleton, not a sprite sheet

Eight headings times every clip times every equipment combination is a
combinatorial explosion of artwork that nobody can author or keep consistent,
and equipment then cannot be mixed at all without redrawing it. Worse, a
sprite sheet is unreviewable by a test: a regression in it stays invisible
until a human looks, so it rots silently.

So a figure is built from **skinned meshes** on a joint hierarchy, filled
through `lib/raster`'s one anti-aliased scan converter. Every property that
can be stated as a number is stated as one, and the ones that cannot are left
for the art harness's contact sheets.

## One body frame serves every heading

A part carries a position in the figure's own frame — `forward` along its
heading, `side` across it, `up` away from the ground — and the projection
turns that frame by the heading, foreshortens its depth axis, and leaves
height alone:

```text
across = forward·east + side·south
into   = forward·south − side·east

dx     = across
dy     = into · FORESHORTEN − up
depth  = into
```

`east` and `south` come from the simulation's own `Facing`, so a figure and
the body that moves it cannot disagree about which way round the world is.

Parts then paint **far-first** by `depth`. Facing away, a face sorts behind
the skull and is simply covered; facing the camera it comes forward. Nothing
asks which way the figure points, so sixteen headings need no new artwork and
no `cfg`. A depth tie breaks on the rig's own part order, so a piece of piping
authored before the flap it edges stays behind it however the figure turns,
and equipment always follows the body it is worn over.

The sort key is deliberately not the screen row. A raised hand draws higher
without becoming further away, so sorting on the row would put it behind the
body it belongs to.

### The elevation is the figure's, not the camera's

WinterSun's world projection is orthographic, axis-aligned and top-down: one
pixel spans a power-of-two number of world sub-units on both axes, and the
ground is drawn unforeshortened. A figure standing in that world is a
billboard seen from a *shallower* angle — about 35°, carried by the single
`FORESHORTEN` ratio — because a part offset along the heading would otherwise
read as a figure lying on the floor.

That is why the constant lives with the figure rather than beside the camera,
and it is also why a figure does not change size with depth: the world
projection is orthographic, so a figure whose size tracked its ground row
would grow and shrink as the camera scrolled.

### Why nothing needs a screen angle

Every vertex of every surface goes through this projection itself, so a yaw, a
pitch and a roll all reach the screen exactly, at every heading. There is no
per-part angle to derive and therefore none to get wrong — which is the whole
of the next section.

## A joint that bears a limb carries its mass

A part names a joint rather than a position. So the shoulder cap the trunk
shows and the arm that swings from it are the *same joint's*, from one entry,
and no posture can part them. In the humanoid there is no separate shoulder
and upper-arm joint, and no separate hip and thigh joint, for exactly this
reason.

Two checks at assembly carry the rule:

- **A joint that bears a child must carry a part of its own.** Without it,
  whatever hangs there grows out of thin air — legs floating below the body is
  the defect this closes.
- **No child's origin may lie beyond everything its parent draws.** This test
  is one-sided on purpose: a shape's reach is an outer bound, so exceeding it
  *proves* a gap, while clearing it does not prove a seam. Proving the seam is
  a measurement over rendered pixels, which is the art harness's job; this
  catches the limb authored nowhere near its socket.

## A posture cannot be illegal

Every joint documents a closed rotation interval per axis, always containing
rest, and `Posture::set` refuses a rotation outside it. A clip that would open
an elbow past straight or fold a knee forward therefore fails *where it is
authored*, not on the frame that drew it — and the placement pass has no
out-of-limit case to handle at all.

The signs follow from one rule, because the transform flips nothing: a part
hangs *below* its joint, so it turns opposite to the joint's own `forward`. A
positive pitch leans the top forward and swings a hanging limb backward; a
positive roll leans the top to the figure's right and swings a hanging limb to
its left. An outward splay is therefore a different sign on each side, which
is why the humanoid's shoulder and hip roll limits are handed — a shared limit
would let one arm bend into the ribs.

## A part is a skinned mesh, not a billboard

A part is a run of **cross-section rings** along a spine. Every ring is
carried by the joint chain in three dimensions and then projected vertex by
vertex, so a limb's far end *is* its child joint, at whatever length and angle
the heading leaves it — exactly, everywhere, with nothing approximated.

The flat outline this replaced could not manage it, and the art harness
measured how badly. A billboard has to be *placed*: an origin, a screen angle,
and a length. The angle was a projection of a three-axis rotation onto the one
axis a billboard can turn about, and its sense was inverted; the length was
the bone's own while the projection shortens a limb pointing into the scene by
twenty to thirty per cent. Against the shipped walk, a thigh's drawn end
missed the knee it hangs from by up to a third of the figure's height:

```
facing east:  thigh drawn end (-12.07,-37.72)   knee at (+12.07,-37.72)
facing south: thigh drawn end (  9.00,-28.00)   knee at (  9.00,-22.97)
```

### Skinned, so a joint bends rather than creases

A ring states how far it is carried by the part's **end** joint rather than
its own. The last ring of a spanning part is carried wholly by the far joint,
which is what puts the surface's end exactly where that joint is; the rings
before it take the blend, which is what makes the bend smooth rather than two
rigid tubes meeting at an angle. A gated test holds every seam rigid across
eight headings and six poses.

### Drawn as shaded strips

The visible half of each ring — solved in closed form from the camera
direction rather than searched for — is split into four arcs, and each becomes
one closed strip down the part, filled at the tone its own surface normal
takes from the light. A cylinder then reads as a cylinder.

The tone is rounded to a fixed ladder, so the whole figure stays drawable in a
small, exactly-known set of colours. That is what lets the art harness check
the palette by equality and count separable regions rather than search for
them.

### Every free end is closed

An open tube shows its own near rim as a crescent where the surface should
have ended: at the crown of a head that reads as a notch cut out of it, and at
a shoulder as a wing. Capping is data rather than code — the first and last
rings of an exposed end taper to a point.

### What it costs

Twenty-one body surfaces trace 688 outline points against the billboard's
468, and the richest figure in the grid, with every feature it can carry,
1,048 — filled through the existing scan converter with no depth buffer and no
allocator. Strips are stored in the converter's own sub-pixel units, which is
both half the memory of a pair of reals and exactly what the painter hands it;
a figure's whole buffer set is then stack-resident, which is what the
cross-target verticals need. The digest's deepest path — a designer's preview
built while the grid's own figure is held — needs about 114 KiB of stack at
the images' `opt-level = 1` (bisected on a bounded thread; the grid alone
needs 81), so each QEMU image that draws it reserves a 256 KiB boot stack —
the `virt` linker scripts on aarch64 and riscv64, and on x86_64 an image
script that sets `BOOT_STACK_BYTES` and includes the shared layout — rather
than relying on the allowance the boot pipeline sizes for itself.

## Equipment is parts, not paint

A helm is its own small mesh mounted on the head socket, not a redrawn head. Gear is stated against a *socket* rather than a joint, so one
piece fits every rig offering that socket and no gear knows a skeleton; the
rig states how gear rests at each mount, so a scabbard angled across the back
is the rig's statement rather than the scabbard's. Gear naming a socket the
rig does not offer is refused rather than silently dropped — a helm that
vanished would read as a missing asset rather than as a rig with no head.

The closed socket set is the weapon and off hands, the head, the back, and a
handed shoulder, hip and foot.

## The humanoid

The one skeleton every species stands on is eighteen joints — a humanoid's
seventeen and a tail's root, which a figure without a tail leaves bare — and
twenty-one body surfaces, plus up to twelve for the features a record asks
for. It is proportioned in percentages of the reference figure's standing
height, so an offset reads directly as a fraction of the figure — a shoulder
at 82, a knee at 28 — and a reviewer can check a proportion without
converting anything. The two sides are mirrored from one pass, because two
tables would be two things to keep in step.

Drawing a figure at a size is one number: the factor is the pixel height a
caller wants over the figure-local units the rig is built in, and it carries
the offsets and the surfaces together, so a figure drawn at half size is half
the figure rather than a full-size arrangement of half-size parts. How tall
the figure itself stands is its record's, below.

## An animation is authored in parameters, not rotations

A pose is a named set of scalars — how far an elbow is folded, how far a hip
has swung, how far the spine has bent — and a clip keys those rather than
joint angles. Each scalar is a fraction of that joint's *own* documented
travel, scaled into the interval by the rig's drive table, and three things
follow from that one decision.

One clip plays on any rig declaring the same parameters, because the clip
names no joint and no angle. The handedness of an outward splay is stated
once, in the drive table, rather than in every clip that lifts an arm. And no
value of any parameter can leave a limit: a bent-backwards elbow is not a
pose that gets rejected, it is one that *cannot be spelled*, because the
elbow's parameter runs from straight to fully folded and has no other end.

That last guarantee is structural rather than checked, and three rules hold
it up. A rigging refuses two drives on one joint axis, so no two parameters
can sum past it. No easing overshoots, so an interpolated value stays between
the two keys it lies between. Blending is a weighted mean, so it stays
between its inputs. A posture built from a pose therefore has no
out-of-limit case at all — the only clamping anywhere absorbs the last bit of
floating-point rounding on a value already mathematically inside its range.

Root motion is deliberately not a parameter: a pose is articulation, and
moving the whole body is a rigid transform of it. Where a dodge's
displacement and a slope's lean are decided is the terrain solve, since
neither can be known without the ground the feet are on. The one part the
ground cannot answer for is how high the body is when *no* foot is down, and
that the clip states itself — see the root-height curve below.

## A clip is keyed against a phase, not a clock

A clip keys each parameter against a phase in `0..=1` and carries its
duration separately, so retiming it is one number and — more importantly —
the phase can later be driven by *distance travelled* instead of by a clock
without the curves knowing the difference. That is what stops a walk cycle
sliding when the speed changes.

Easings are the non-overshooting ones: linear, ease in, ease out, ease in and
out, and a hold that steps at the next key. The omission is deliberate. An
overshooting easing would break the in-range guarantee above, and the snap of
a recoil and the settle of a follow-through are damped layers over the clip,
where they can be bounded on their own terms rather than smuggled into a
keyframe.

A clip also carries **named events at phases** — a footstep, a hit frame, an
arrow loosed — which is the seam the game's timing is built on: a hitbox
opens on the frame the art shows it rather than on a timer that drifts from
it. Asking a clip which events a phase step crossed is half-open, so an event
fires exactly once as the phase passes it and never twice, and a step that
laps reports the tail of the clip before the head of the next in the order
they actually happen. The engine carries the name and the phase and learns
nothing about what either means.

## Blending is weighed per parameter

Weight accumulates against each *parameter*, not against each pose, and that
is the whole reason a mask means anything: a parameter is weighed only
against the clips that had an opinion about it. A cast playing on the arms
therefore does not drag the legs toward rest by the weight it was mixed in
at.

A parameter nothing wrote resolves to rest, which fixes how partial
animations compose: an overlay covering part of the body is layered *over* a
base covering the rest — added to the same blend — rather than cross-faded
against it. Cross-fading a full-body clip out from under a partial one would
leave the uncovered half at rest as the base's weight reached zero.

## The transition machine is checked before it can strand anything

States name clips, edges carry the seconds one state takes to become another,
and the whole graph is checked once when it is assembled: every clip present,
every cross-fade a positive number of seconds, every state reachable from the
initial one. A machine that could strand a figure in a state nothing leads to
is refused at load rather than discovered when it happens. Edges are held
ascending, which makes a state's outgoing edges a contiguous run — so a
lookup is a search rather than a scan, and a repeated edge cannot be spelled.

Which state to be in is the simulation's business, not the engine's: a
consumer maps its own notion of grounded, speed, action and stagger onto a
state and asks for it, and asking for a state with no edge from where the
figure is gets refused. At most two clips are live at once; asking for a
state mid-fade replaces the outgoing clip rather than queueing a chain of
fades that would take longer to settle than the input that caused them.

A fade carries the root height with the pose: `Animator::root` weighs each
live clip's own height exactly as `Animator::blend` weighs its curves, so a
figure fading from a standing idle into a crouching run passes through the
heights between rather than dropping when the fade begins or ends.

## Costs nothing to draw

Nothing in the crate allocates. A rig holds fixed arrays; a posture is one
rotation per joint; and the placement buffer is caller-held and sized to the
largest figure, so a scene of figures costs no allocation at all and a joint
whose rotation is zero on an axis skips that axis entirely.

The joint and part bounds are bounds on *authored content* rather than runtime
capacities: a rig is first-party code, not input, and the shipped rig's counts
are held to them at build time. Runtime-loaded figure geometry from an
untrusted source is refused by design; only a figure's *parameters* are
validated data — the record, below.

## The layers over a clip

A clip on its own looks keyframed. What sits above it is a short stack of
pure functions of the pose, the state and the time, each with a property a
host test holds it to rather than a screenshot a human squints at.

Every layer but one states its effect as a signed **delta per parameter** — a
`pose::Overlay` — rather than as a value. Deltas sum, so the order two layers
run in cannot change the answer, and the sum is brought back inside each
parameter's range on the way out. That is what carries the in-limit guarantee
through the whole stack: a delta is a further fraction of the joint's travel,
and a layer asking for more than is left fades rather than fighting the pose
already there.

The order is the contract. The animator picks clips and blending resolves a
pose; breathing, look-at and recoil add their overlays; the planting solve
consumes *that* pose and answers the final one, because it must re-aim legs
the layers above have finished with; and the placement draws the result.

### The gait is driven by distance, and its stride is measured

A cycle advanced by a timer slides: change the speed and the feet keep their
old cadence, so they skate, and a figure held still walks on the spot.
`gait::Gait` advances the phase by the **distance travelled** instead, so a
foot is planted as a function of position and cannot move while the ground
under it does not. The same ground covered gives the same phase however it
was divided into frames.

That leaves the stride — how far one cycle carries the figure — and authored
by hand it is a guess that puts the slide straight back. `Gait::fitted`
measures it from the clip and the rig: it finds the span of the cycle the
foot is on the ground for, and asks how far the foot travels backward through
the body over it, which is exactly how far the body must travel forward for
the foot to stay still. `Gait::slide` then reports the residual, so "the feet
do not skate" is a bound rather than an opinion — on the walk the tests
author by solving the leg, the fitted stride recovers the authored one and
the planted foot moves under a hundredth of a stride over the ground.

Where the foot is down is read over the ground, at the height the clip holds
the body, not through the body frame. A stance that sinks the body while the
leg folds into it keeps the foot on the floor but raises it through the body,
and a window read there holds only the ends of the step: on the shipped run it
straddled the swing and fitted a stride twelve per cent short.

### Each foot on its own ground, not on the ground's average

The world has real slopes, so a figure standing across a gradient has one
foot higher than the other. Drawn from the root's height both sit level, and
the camera looks straight down at the line where they meet the ground — the
uphill foot floats and the downhill one sinks. This is a correctness
requirement rather than polish: the error is worst exactly where the eye
already is.

`plant::Legs` solves it. A leg cannot stretch, so the lowest ground is the
constraint: the root drops until that leg reaches it, and the other takes up
the difference by folding. Each foot keeps the plan position the animation
gave it and changes only its height, so a walk still swings its legs where
the clip said — the hip is re-aimed and the knee re-folded to put the ankle
at the new height, and the ankle turns back by as much as the leg above it
turned, so a toe-off stays a toe-off.

Each foot is asked for the height the clip put it at, raised by the terrain
beneath it, so a planted foot lands and a swing foot keeps its arc without
either having to be told apart from the other. On flat ground every target
*is* the ankle the animation already produced, so the whole solve is an
identity and a figure on the level is drawn exactly as its clip authored it.
That property is a test, and it is the one that stops the planter quietly
redrawing every figure in the game.

The ground a foot is raised by is the ground the view *draws*. WinterSun draws
its world from directly above, with height shown by shading alone, so the plane
a figure's feet meet on screen is level everywhere and the game's actor plants
on the level; a view that draws relief as geometry — an elevated camera —
plants each foot on the heights it draws. Standing water is the exception that
shows from above: a figure wading stands on the bed, sunk by the depth, and
nothing of it is drawn below the surface.

### The height the body is at is the clip's, not a reading of the fold

Both legs folded is a deep crouch and a run's flight phase at once, so no
reading of the articulation tells the two apart. Inferring the height from
the *lesser* fold — which is what the planting layer used to do — gets a
stance right and a flight exactly wrong: it sinks the figure by the tuck at
the moment it should be rising, which on the shipped run measured a little
over a unit in a hundred, in the wrong direction, over the quarter of the
cycle with neither foot down.

So a clip carries a root-height curve (`clip::Lift`), dimensionless like
every other authored animation value — a fraction of a straight leg, so the
same curve holds on a taller rig. Zero is where a straight leg puts the sole
on the ground; negative is standing into the legs; positive is off the ground
altogether. A displacement past a whole leg either way is a move the
simulation authorised rather than a cycle's own rise and fall, and is
refused. The idle and the walk keep a foot down at every phase and so hold
one height throughout. The run's body sinks from each strike to midstance and
rises again to toe-off — the stance leg folding into the landing and
straightening out of it — then follows a parabola across the flight, meeting
the stance height at both ends so the height never steps where a foot takes
over. The dip is as deep as the flight rises, so the bob is centred on the
height the body lands at and spans a twentieth of the figure. Its ends are
flat rather than at the flight's slope: that join would have the leg
shortening at the landing's full speed as it strikes, and keys a thirty-second
of a cycle apart cannot follow it without the planted foot sinking past the
grounding bound — even a fifth of the shipped rise fails it on the
longest-legged build a record describes.

The run's height is keyed at its leg keys' own phases. Between two keys both
are interpolated linearly, so the body and a planted foot move along one line
and the foot stays on the floor; keyed any finer, the height follows its true
curve while the legs facet, and the difference sinks the foot.

Nothing in the solve makes a clip's stated height agree with its own leg
keys — a clip claiming to stand upright while folding its legs would put its
feet through the floor. That agreement is a *measured* property instead:
over a whole cycle the lowest either foot ever reaches has to be the floor
exactly, since below it the foot sinks in and above it the figure never lands.
The two are one quantity's two signs, so `quality::grounding` answers both
with one number, and it needs no notion of which foot is "down" — a contact
band widens near a foot's lowest point, where its height is flat, and would
report a foot planted well into its own toe-off. What it measures on the
shipped set is the sag between keys: a leg's angles are interpolated
linearly, so the foot they put down arcs slightly below the line its path
holds it to, deepest halfway between keys. Six-place rounding adds under a
hundred-thousandth of a unit to that.

Every shipped leg key is its foot path solved: a test states each clip's path
whole — strike distance, stance fraction, swing clearance, and how much of the
leg's turn the ankle levels the foot by — puts it through the planting layer's
own two-bone solve, and holds every key of every table to the six places it is
written to.

How much height difference the legs can absorb is the rig's own statement —
the span between a straight leg and a fully folded one, which for the shipped
humanoid is about thirty units against an eighteen-unit stance, a slope of
fifty-nine degrees. Past that the figure leans into the hill instead, and
what even the lean cannot reach is *reported* as a miss rather than fudged,
because a figure standing somewhere no figure could stand is the simulation's
defect to see.

### The root is the placement's, not the pelvis joint's

A lift, a crouch's drop and a slope's lean are a rigid transform of the whole
body, carried on the `Stance` and seeded into the resolve as the frame the
parentless joints hang in — so it costs the resolve nothing beyond the value
it already inherits, and every surface carried through that resolve picks it
up with no second path.

Rolling the pelvis *joint* instead was measured and rejected twice over. Its
roll limit is 0.20 rad, which across an eighteen-unit stance absorbs 3.58
units of height difference — a slope of 11.2 degrees, against the 59 the
planting reach already handles. A fallback that saturates five times earlier
than the thing it backs up is not one. Independently, a pelvis roll pivots
about the pelvis and swings the feet sideways *through* the ground, where a
whole-figure lean must pivot about the ground contact — which is the body
frame's origin, and so the placement's.

### One spring, solved rather than integrated

Recoil, follow-through and the sway of a cloak are all a value pulled toward
a target and resisted in proportion to its speed, so there is one
`spring::Spring` and no second curve to drift from it. It steps by evaluating
the oscillator's closed-form solution over the interval rather than
integrating: a spring stepped by Euler gains energy when the frame is long
against its own period, and the frame that arrives late is exactly the one on
a loaded machine. The envelope here is at most one for any non-negative step,
so a stall produces a settled figure rather than a detonated one, and
stepping twice over half an interval gives the same answer as stepping once.

A recoil is an **impulse**, not a displacement: the hand has not moved yet on
the frame the blow lands, and it is the speed it picks up that reads as
weight. Its target is the animation itself, so it vanishes completely once
spent, and the overshoot on the way back — a damping ratio below one — is the
follow-through whose absence makes an attack feel weightless. Sway is driven
by the carrier's **acceleration** rather than its velocity, so a figure moving
steadily has its cloak hanging straight behind it and one that starts, stops
or turns throws it; wind adds to the same drive with no second path. Gear
takes the sway as a turn on its socket; a tail, which hangs from a joint of
its own, takes it through its two parameters as an overlay like any other
layer's, so it sums with a clip and stays inside the joint's limits.

### Look-at, breathing, root motion, and the shadow

Look-at turns head and spine toward a target as a delta over whatever clip is
playing, splitting the turn between the two so the shares always sum to the
whole — the split decides how the figure looks doing it, not where it ends up
looking. A target past the neck's travel is not refused: the figure turns as
far as it can and the range clamp stops it there.

Breathing is a slow cycle too small to read as an animation and impossible to
miss when it stops, which is what keeps an idle figure from reading as a
paused game.

Root motion is the one thing a clip may say about *movement*, and it says only
the pacing: a `clip::Travel` is the fraction of the move spent against the
phase, running from none of it to all of it, and the simulation multiplies
that by whatever displacement it actually authorised. A client cannot move
itself by playing an animation, and the animation and the movement cannot
disagree about how the move was paced.

The contact shadow is a circle on the ground raked away from the light and
then foreshortened on the way to the screen. Those are two different
stretches, so the result is an ellipse whose axes are neither — taking the
light's stretch as the screen's puts the long axis visibly wrong at every
bearing but four. `shadow::Contact` composes the two maps and recovers the
axes of what comes out, which is exact at every bearing for a handful of
arithmetic. A rising figure's shadow stays on the ground and slides away from
the light as it thins, which is the cue that reads as height rather than as
the figure growing. Drawn soft, it is three nested rings, the light's raked
footprint the innermost, each light enough that where all three overlap they
compose to the hard shadow's own density; drawn hard, it is the one ellipse the
art harness measures.

## An actor plays a figure for the simulation

`actor::Actor` turns what the simulation says — where a body is, which way it
faces, what it is doing, how deep the water it stands in — into a pose, frame
by frame. It never moves itself: every input is the simulation's, read between
its ticks.

The shipped motion set is seventeen clips in three families, each with the
layer it plays on and what holds the figure up while it does:

| Family | Clips | Layer |
|---|---|---|
| Locomotion | idle, walk, run | under everything |
| Actions | dodge, melee heavy, stagger | the whole body |
| | melee light, draw, loose, cast, channel, hit | the upper body |
| States | fall, die, sit, swim, climb | the whole body |

**Locomotion is chosen, not blended.** The walk and the run are authored with
different stances, and their joint angles mixed at a weight set by speed put a
foot where neither clip does — measured across the speeds between their paces,
such a blend sank a planted foot half a unit into the floor and slid it by
several. So one gait plays at a time, chosen by the body's speed with a margin
either side of each change so a body at the edge does not flicker, and paced by
the distance covered at that gait's own fitted stride. A change of gait fades
between the two over a quarter of a second at the one phase their feet share.
A test holds a planted foot still at every steady speed from a third of the
walk's pace to over three times it.

**An action's timing comes from outside.** An action is windup, active and
recovery; its clip is authored across the three in phase, the phases they meet
at are stated beside it, and `clip::Timing` stretches each segment to the
seconds the action that owns it asks for. The durations shipped are each
action's reference timing, which WinterSun's action documents replace, so the
art and the rules cannot disagree about how long a blow takes to land.

An action layer fades in as it takes a clip up and hands the body back by
itself when the action has played through; a state lasts until the figure is
settled. A figure standing still on dry ground for twenty seconds sits, and
moving brings it up. Its shown heading swings toward the body's at a bounded
rate, so an about-face reads as a turn rather than a flip.

`WORLD_SCALE` is the one conversion between a rig's units and the world's: a
reference figure a little over two cells tall, which is the scale at which the
simulation's default pace runs at the run clip's own cadence. The body the
rules collide is the figure's `footprint`, half its width across the arms at
rest, so two bodies the simulation lets touch are drawn touching. `REACH` is how
far any figure draws from its ground point, which a scene culls by, and
`readable` answers whether a view's scale still draws the smallest figure a
record describes at the harness's readability floor — what the game's detail
ladder takes its floor from.

## The art is measured, and the measurements are gated

`cargo xtask artsheet` walks a reference grid — each species' reference figure
in every shipped motion, and each species' least, most and two plausible
figures walking, at eight phases, facing four ways — renders each cell and
holds every number against a bound. Every preset the game ships is measured
too, in every motion at the readability floor, with rows for its record and
its motions and bounds only for its cells. A reference figure is measured at the
three pixel sides the desktop draws a figure at; a walking one at the
smallest, the readability floor, since what a figure costs is counted in
outline points and fill area and neither depends on the side. It runs in
`ci`, and it does two things at once: it regenerates
`userland/games/wintersun/figure/artsheet.ledger` and compares it byte for
byte, *and* it checks the freshly measured numbers. Drift alone would admit a
regression somebody had regenerated; bounds alone would admit a change nobody
noticed.

The committed golden is **text**, not a picture. A committed PNG is
unreviewable in a diff — the very objection a sprite sheet fails on — so the
ledger carries one row per cell, and a change reads as
`skate 0.002718 -> 0.014803`. `--sheets` renders the pictures on demand into
the gitignored `images/artsheet/`, which is what keeps the thing a human
judges current rather than as-of-last-regeneration.

| Measured | Where its bound lives | Shipped worst |
|---|---|---|
| Joint-limit use, verified through `Posture::set` | `figure::quality` | 0.86 of a joint's travel, the draw's |
| Foot skate, as a fraction of the fitted stride — the walk and the run | `figure::quality` | 0.0027, the walk's |
| Motion continuity, per unit of a parameter's or the root height's range | `figure::quality` | 0.041 |
| Loop closure — the looping clips | `figure::quality` | exact |
| Grounding: the lowest foot against the floor, for a figure on its feet | `figure::quality` | 0.070 of a figure's units, a preset elf's run |
| Penetration: no foot below the floor, for a figure the ground does not hold up | `figure::quality` | none |
| Coverage ratio, tonal regions, contrast against both themes | the harness | 0.060–0.163, ≥ 4 regions, ≥ 2.19 |
| Outline points and fill area per cell | `figure::paint` + the harness | ≤ 1,096 points, ≤ 0.21 overdraw |

Which of these a motion is held to follows from what it is — `quality::Measured`
reads it off the motion's kind and support — so a sit is not asked to close a
loop it does not play, and a figure in mid-fall is not asked to touch the
floor.

A cell holds the whole figure. The depth axis draws what is nearer the viewer
lower on the screen, so the near foot of a stride lands below the ground point
the figure stands on — a fifth of its reach below it for the longest-legged
build. A cell is framed by the figure's rest reach with two measured
allowances per motion, above the ground point and below it, between margins of
a fiftieth of the side: 1.02 and 0.20 for locomotion, and never tighter for
any other motion, so no motion's cells draw a figure larger than a stride's
do. A fall reaches further above, an arm raised in front of a figure facing
away is drawn up the screen past its crown, and a sit folds below. Every build
corner of every species stays within its motion's allowances at every
sixteenth of a turn, and every cell of the grid inside its square. Coverage is
restated at locomotion's framing, so a motion framed looser is not read as
the figure drawn thinner. A palette is the player's to choose, so the harness
also draws every dye on every species' palest and darkest build and holds each
cell to the same bounds, without ledger rows, since the grid already carries
the worst of them.

The pose-side measurements live in the crate rather than the harness, so
`cargo test` runs them on every Tier-1 target and a later figure preset is
measured by exactly the code the shipped one was.

## The cross-target claim

A figure is `f64` throughout, over `lib/util::mathf`'s first-party
transcendentals, so bit-identity *follows* from the language rather than from
a convention. `figure::digest::REFERENCE_DIGEST` is the claim that it holds:
the carried rings at full precision, their projections, the drawn strips, the
planting roots and misses, the gait's own phases and the quality numbers, all
folded over raw `f64::to_bits` with no quantisation and no tolerance. A
tolerance would hide the one hazard that is real — a backend fusing a multiply
and an add — which is exactly what the verticals exist to catch rather than
assume.

It is asserted by the host suite and by one vertical per Tier-1 target
(`tests/integration/figure_determinism_*`), so no target can pass by having
never run.

## A figure is a record, and the record is never geometry

A character is an `identity::Identity`: nineteen checked bytes, and a figure
is built from nothing else.

| Byte | Field | What it holds |
|---|---|---|
| 0 | version | the record format; anything but this build's is refused |
| 1 | species | human, elf, dwarf, beastkin or dragonkin |
| 2–6 | build | height, girth, taper, limbs, head — each a setting across its species' interval |
| 7–9 | face, eyes, ears | a face shape, an eye shape, an ear form |
| 10–12 | horns, tail, hair | zero for none, one past the form otherwise |
| 13 | volume | how full the hair is; zero when there is none |
| 14–18 | palette | skin (fur, scale), hair, eyes, markings, cloth — swatch indices |

**Every setting is inside its species by construction.** A build setting is
a byte spanning the whole interval its species documents — a dwarf's highest
height is still a dwarf's — so there is no out-of-range setting to clamp, and
both ends of every interval are exactly its documented ends. The intervals,
the forms each feature may take, and the swatch tables each palette slot draws
from are `species::Species` data rather than code: a species is what it may
be, not a different rig. Every species stands on the one skeleton, so every
clip plays on all of them.

**The decoder is total and fails closed.** In the game the record arrives
from a client assumed hostile, and the server re-validates a submitted figure
with exactly this: every byte string answers a record or an
`IdentityError` naming the field it refused — a byte naming nothing, a form
or eye colour the species does not carry (a tail on a human, a dragon's red
eyes on an elf), a swatch past its table. A figure has **one spelling**: a
bald figure stores zero for its hair's colour and volume, a species without
markings zero for its markings, and any other value is refused rather than
ignored, so two records that draw the same figure are the same bytes. Every
record the decoder admits builds and places a figure — the property the
decoder's fuzz harness holds, beside a regression corpus with pinned
verdicts — so a server's check and a renderer can never disagree about what
is drawable.

**The record reaches the figure through three narrow doors.** Joint offsets,
which are values, so a longer limb is a longer bone. A `mesh::Stretch` on each
surface's ring template — five factors along a ring centre's three axes and
across and through its cross-section — so the rings stay borrowed
first-party tables and a build costs a few multiplies at carry time; owning
them per figure would more than double a rig for nothing. And a role table: a
surface names a `tint::Tint` rather than a colour, so a palette edit is
`Rig::retint` and never a rebuild.

**Height is height.** Limb and head proportion change a figure's shape and not
its stature: the builder scales the whole skeleton so the crown stands exactly
where the height setting puts it, with the sole on the ground. A long-legged
figure of a given height has the shorter trunk, which is what a proportion is.
A test measures crown and sole off the carried rings, for every species at
every one of its build corners, independently of the arithmetic that built
them. Every build keeps the thigh-to-shank proportion the shipped foot paths
were solved through, so the clips stay grounded and skate-free on all of them
— measured, at every corner.

**Features choose templates; they never bend one.** Faces share one cranium
and differ in the jaw, so everything anchored to the crown fits every face.
Hair is a cap over the crown and, where it reaches down the back, a mass
behind: a flat depth sort cannot put one surface both behind a face and over
it. The cap and the skull are sorted by one shared point, so their depths are
always the same number and the cap, authored after, covers the skull from
every side — two means would tie only until the head nodded, and a test
reproduces exactly that. An animal's ear and a tail carry a marked face or tip
in the species' markings colour, and a dragonkin's fin is a membrane drawn in
them too.

**Every palette is readable.** The one colour every figure wears whatever it
chose — the trousers — sits in the narrow luminance band that clears two-to-one
against both the dark desktop and the mid-grey light one on its own, and the
legs are always a substantial share of a figure. The art harness checks that
tone before any cell, and the grid draws every species in its palest and its
darkest covering.

## The designer

A designer is sliders over a record, which makes it the surface most likely to
freeze its own window: wired to a store, a slider writes once per pointer
sample; wired to a rebuild, it re-derives the whole figure per sample for a
change of one colour. `design::Designer` is the model that makes both
impossible to spell.

**An edit is live and costs what it feeds.** `Designer::edit` changes the
record in memory and nothing else. What the next paint owes is
`design::Change::between` the record it last drew and the one now live:

| Change | When | What the preview does |
|---|---|---|
| `Nothing` | the record drawn is the record live | nothing |
| `Tints` | only the palette differs | `Rig::retint`: no point moves |
| `Rig` | the species, the build or a feature differs | a rebuild |

A figure's geometry is its species, build and features, and its colours its
species and palette, so the diff is exact. Diffing against what was *drawn*
rather than against each edit is what makes a burst of edits between two
frames cost one catch-up.

**The durable write happens once, where the interaction settles.**
`Designer::settle` answers the record to store when the store does not hold it
already, and nothing otherwise: a drag of any length is at most one write, and
one that ends where it began is none. The store is the surface's to reach, off
the frame loop.

**An answer lands on what the player is not doing.** One write is out at a
time, and its answer can arrive during the next drag. `Designer::landed` takes
the store's record, and `Designer::refused` the one it kept, in every field
the player has not edited since the write went out; the drag in hand stays
theirs, so an answer never snaps a slider back under the pointer. A settle
made while a write is out is owed, and whichever answer arrives hands it out
— one write for however many settles it covers.

**The record is always one.** The designer keeps what the player chose, field
by field, and the record those choices come to for the species chosen. An edit
the species cannot carry is refused with the field the record decoder would
name, and changes nothing. A species change or going bald re-derives what it
reshapes: a swatch past the new species' table is clamped to its last, a form
the species lacks becomes its first, a form it must carry is given — a
dragonkin always has horns and the scaled tail — an eye colour it does not
admit becomes the admitted one nearest in colour, and a bald figure's hair
colour and volume are zeroed. The choices survive beneath the record — an edit
to a field the record holds at zero can only ask for that zero, and leaves the
choice beneath alone — so going to an elf and back, or bald and back, gives
back exactly the figure you left.
This is not a repair of a record: a record arriving from anywhere else is still
decoded and refused, never adjusted.

**The preview plays, on the harness's own stage.** `preview::Preview` plays
the shipped motions through the transition machine — choosing a clip
cross-fades into it — breathes, and stands on the grid's stage: the same
ground, light and pose-to-placement path the harness measures. A view is framed
one of two ways, and a designer shows both at once. `Frame::Measured` fills its
square as the harness frames a cell, so the small view beside the large one is
the readability floor itself. `Frame::Shared` draws every figure at the one
scale the largest figure a record describes fits (`humanoid::MOST_REACH`):
height is a scale of the whole skeleton, so a figure filling its own square is
the same size at every height, and a height slider needs the shared frame to
show anything. A catch-up never touches the clock, the clips playing, the
breath or the heading, so no edit restarts the animation it is judged by.

**A preset is a record.** Picking one is `Designer::apply` and a settle. The
preset set is `WinterSun` bundle content, not a table in this crate.

**A random figure is plausible, not uniform.** `plausible::figure` draws each
setting from a bell about the middle of its species' interval, so an end is
reachable and rare; a heavy build leans broad-shouldered, and a tall one
long-limbed and small-headed for its height. Hair and markings lean toward the
lightness of the skin, fur or scale they grow from, so a pale figure has pale
markings, and the tunic leans away from it, so the two read apart. A form a
species may go without is carried as often as the species is described as
carrying it. The draw is integer-only and takes an injected generator; the
predictable `NonCryptoRng` is the one to give it, since a figure a player could
have built by hand protects nothing by being unpredictable and a seed that
names one figure is what the tests and the grid need. Two seeded draws per
species are figures of the art grid, so what "surprise me" hands a player is
held to every bound an authored figure is.

## What comes next

The engine's own items are done and its figures are on screen (WS6); what uses
it next is `WinterSun`'s designer surfaces (WS17), and the action documents
(WS9–WS11) that replace the actions' reference timings. Two
decisions are recorded rather than taken (`plans/FIGURE.md`): the sheets draw
eight phases, so no picture shows a figure mid-flight or at the bottom of a
run's stance; and the run's stance meets its flight flat, since joining at
the flight's slope needs leg keys finer than a thirty-second of a cycle, and a
measurement grid finer still to see between them.
