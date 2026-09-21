# The WinterSun figure engine

`userland/games/wintersun/figure` (`tairix-wintersun-figure`) answers what a
character *is* before anything animates it: a skeleton of joints, the
parametric parts bound to them, the sockets equipment hangs on, and the one
projection that turns all of it toward the camera. It is `plans/FIGURE.md`
FG2, and the sixth crate of the `userland/games/` leaf subtree. Stability
tier: **experimental**.

## Parts on a skeleton, not a sprite sheet

Eight headings times every clip times every equipment combination is a
combinatorial explosion of artwork that nobody can author or keep consistent,
and equipment then cannot be mixed at all without redrawing it. Worse, a
sprite sheet is unreviewable by a test: a regression in it stays invisible
until a human looks, so it rots silently.

So a figure is built from the six parametric outline primitives `lib/raster`
owns, placed on a joint hierarchy, and drawn through the one anti-aliased scan
converter. Every property that can be stated as a number is stated as one, and
the ones that cannot are left for the art harness's contact sheets.

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

### Why every outline is symmetric about its vertical axis

A rotation about the frame's `up` axis lies in the ground plane, and this
projection turns a ground-plane rotation into a *shear* rather than a screen
rotation. An outline symmetric about its own vertical axis is unchanged by
that shear, so a head's turn needs no outline change at all. An asymmetric
outline would need mirroring, and mirroring is exactly the per-direction
branch the body frame exists to avoid.

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

## What a screen turn can and cannot be

A flat outline cannot carry a rotation in three axes, so the placement takes
exactly the part of it a screen can show: the joint's rotation vector,
projected onto the two axes a screen rotation is visible about. A fore-and-aft
swing shows in full seen from the side and not at all seen down the depth
axis, where it happens in depth. The angle is recovered from the frame itself
rather than estimated, so a raised arm turns as far as it actually went.

The alternative — measuring the screen angle of the joint's projected up axis
— is exact but flips through half a turn where that axis crosses the view
direction, and a limb that pops is worse than one that turns a few degrees
short. The projection of the rotation is the deliberate choice.

## Equipment is parts, not paint

A helm is a bevelled panel and a wedge mounted on the head socket, not a
redrawn head. Gear is stated against a *socket* rather than a joint, so one
piece fits every rig offering that socket and no gear knows a skeleton; the
rig states how gear rests at each mount, so a scabbard angled across the back
is the rig's statement rather than the scabbard's. Gear naming a socket the
rig does not offer is refused rather than silently dropped — a helm that
vanished would read as a missing asset rather than as a rig with no head.

The closed socket set is the weapon and off hands, the head, the back, and a
handed shoulder, hip and foot.

## The humanoid

The first-party rig is seventeen joints and twenty-one parts, proportioned in
percentages of its standing height so an offset reads directly as a fraction
of the figure — a shoulder at 82, a knee at 28 — and a reviewer can check a
proportion without converting anything. The two sides are mirrored from one
pass, because two tables would be two things to keep in step.

Scaling is one number: the factor is the height a caller wants over the rig's
authored standing height, and it carries the offsets and the outlines
together, so a figure drawn at half size is half the figure rather than a
full-size arrangement of half-size shapes.

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

Root motion is deliberately not a parameter. A jump's lift, the pelvis drop
of a crouch and a dodge's displacement cannot be decided without the ground
the feet are standing on, so they belong with the terrain solve rather than
split across two items.

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

## Costs nothing to draw

Nothing in the crate allocates. A rig holds fixed arrays; a posture is one
rotation per joint; and the placement buffer is caller-held and sized to the
largest figure, so a scene of figures costs no allocation at all and a joint
whose rotation is zero on an axis skips that axis entirely.

The joint and part bounds are bounds on *authored content* rather than runtime
capacities: a rig is first-party code, not input, and the shipped rig's counts
are held to them at build time. Runtime-loaded figure geometry from an
untrusted source is refused by design; only a figure's *parameters* are
validated data, and those arrive with a later item.

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

### Each foot on its own ground, not on the ground's average

The world has real slopes, so a figure standing across a gradient has one
foot higher than the other. Drawn from the root's height both sit level, and
the camera looks straight down at the line where they meet the ground — the
uphill foot floats and the downhill one sinks. This is a correctness
requirement rather than polish: the error is worst exactly where the eye
already is.

`plant::Legs` solves it. A leg cannot stretch, so the lowest foot is the
constraint: the root drops until that leg reaches its ground, and the other
takes up the difference by folding. Each foot keeps the plan position the
animation gave it and changes only its height, so a walk still swings its
legs where the clip said — the hip is re-aimed and the knee re-folded to put
the ankle at the new height, and the ankle turns back by as much as the leg
above it turned, so a toe-off stays a toe-off.

On flat ground every target *is* the ankle the animation already produced, so
the whole solve is an identity and a figure on the level is drawn exactly as
its clip authored it. That property is a test, and it is the one that stops
the planter quietly redrawing every figure in the game.

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
it already inherits, and the projection and the screen turn pick it up with
no second path.

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
or turns throws it; wind adds to the same drive with no second path.

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
the figure growing.

## What comes next

The contact-sheet harness that makes readability, palette conformance, joint
limits, foot slide, motion continuity and loop closure measured, gated
properties rather than opinions; then the species and build parameter space,
and the designer that drives it.
