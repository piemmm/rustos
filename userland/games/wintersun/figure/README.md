# tairix-wintersun-figure

WinterSun's figure engine: the rig a character exists as, the skinned meshes
it is drawn from, the clips that move it, and the procedural layers that stop
it looking keyframed — a parent-relative joint hierarchy with per-axis
rotation limits, surfaces skinned across the joints that carry them, named
equipment sockets, the depth-sorted draw order, the one body frame that serves
every heading, and over all of it a gait driven by distance travelled,
per-foot terrain planting, look-at, recoil, sway, breathing and a contact
shadow — plus the shipped motion set and the measurements the art is gated on
(`plans/FIGURE.md` FG2–FG5).
Stability tier: **experimental** — nothing has shipped, so a type changes in
place until it does.

`no_std`, no allocator, `forbid(unsafe_code)`.

## Why a rig and not a sprite sheet

Eight headings × every clip × every equipment combination is artwork nobody
can author or keep consistent, and a sheet is unreviewable by a test: a
regression in it is invisible until a human looks. So a figure is a skinned
mesh on a skeleton, filled through `lib/raster`'s one anti-aliased scan
converter, and every property that can be stated as a number is.

## Three ideas

**One body frame serves every heading.** A part carries a `Body` offset —
along the figure's heading, across it, and away from the ground — and
`frame::project` turns that frame by the heading and foreshortens its depth
axis. Parts then paint far-first by projected depth, so facing away a face
sorts behind the skull and is covered, and facing the camera it comes forward.
No front/back/side artwork, no per-direction branch, no second path to keep in
step. The depth key is deliberately *not* the screen row: a raised hand draws
higher without becoming further away.

The foreshortening is the figure's own drawing elevation, not the world
camera's. WinterSun's ground is drawn top-down and unforeshortened
(`wintersun/app`'s `Camera`); a figure standing in it is a billboard seen from
about 35°, which is why this constant lives here rather than beside the
camera.

**A joint that bears a limb carries its mass.** A part names a `JointId`, so
the shoulder cap the trunk shows and the arm that swings from it are the same
joint's — no posture can part them. `Rig::new` refuses a rig where a joint
bearing a child carries no part of its own, or where a child's origin lies
beyond everything its parent draws. Legs floating below the body is the defect
this closes.

**A part is a skinned mesh, not a billboard.** A run of cross-section rings
along a spine, each carried by the joint chain in three dimensions and then
projected vertex by vertex. A limb's far end therefore *is* its child joint,
at whatever length and angle the heading leaves it. The flat outline this
replaced had to be placed from an origin, an approximated screen turn and the
bone's own unforeshortened length, and could not: measured against the shipped
walk, a thigh's drawn end missed its knee by a third of the figure's height.
The last ring of a spanning part is carried wholly by the far joint, which is
what makes the seam exact; the rings before it take the blend, which is what
makes the bend smooth.

**A posture cannot be illegal.** Every joint documents a rotation limit per
axis, and `Posture::set` refuses a rotation outside it — so a clip that would
bend an elbow backwards fails where it is authored rather than on the frame
that drew it, and `place` has no out-of-limit case to handle.

## Sign conventions, once

Rotations are right-handed about the body axes, with no sign flipped anywhere
in the transform. The consequence that decides the sign of every limit: a part
*hangs below* its joint, so it turns opposite to the joint's own `forward`. A
positive `pitch` leans the top forward and swings a hanging limb **backward**;
a positive `roll` leans the top to the figure's right and swings a hanging limb
to its **left** — so an outward splay is a different sign on each side, and
the humanoid's shoulder and hip limits are handed accordingly.

Nothing anywhere needs a screen angle for a part: every vertex goes through
the projection itself, so a yaw, a pitch and a roll all reach the surface
exactly, at every heading, with no approximation to get the sign of.

## Modules

| Module | What it owns |
|---|---|
| `frame` | The body frame: `Body`, `Rotation`, the orthonormal `Basis` and its composition, the projection, and the camera direction a silhouette is taken against. |
| `joint` | `JointId`, the checked `Limit`/`Limits` intervals, and `Joint` — a parent, a rest transform, and how far it turns from it. |
| `socket` | The closed socket set equipment hangs on, and where a rig mounts each one. |
| `mesh` | `Ring`/`Hoop`: a part's cross-sections, how the joints carry and skin them, the near arc a silhouette is taken over, and the shading ladder a strip is filled at. |
| `rig` | `Rig` and its validation, `Posture`, `Part`/`Fitted`, and the `Placement` a figure is skinned, projected and depth-sorted into. |
| `humanoid` | The first-party humanoid: 17 joints, 21 skinned surfaces, every socket, proportioned in percentages of `STANDING_HEIGHT`, and the drive table binding the pose parameters to them. |
| `motion` | The shipped motion set — idle, walk, run — whose leg curves are a stated foot path solved through the same two-bone geometry the planting layer uses. |
| `paint` | The one paint order (shadow, then strips far-first) and what drawing a figure costs. |
| `quality` | The measurements the art is gated on — joint-limit use, motion continuity, loop closure, foot skate — each with its bound beside it. |
| `reference` | The reference grid the cross-target digest folds and the art harness draws: which figure, in which poses, facing which way. |
| `digest` | `REFERENCE_DIGEST`: the cross-target claim, asserted by the host suite and one vertical per Tier-1 target. |
| `pose` | `Param` — the 22 named scalars an animation is authored in — the `Pose` holding them, their `Range`, and the `Mask` a blend writes through. |
| `rigging` | `Drive`/`Rigging`: which joint axis a parameter turns, and which way its `+1` points. |
| `clip` | `Clip`: a keyed `Curve` per parameter with an `Easing` per segment, a duration, a `Loop` mode, and the named `Event`s at phases along it. |
| `blend` | `Blend`: weighted accumulation of poses and clips, weighed per parameter so a mask means something. |
| `transition` | `Transitions` — states, clips and per-edge cross-fades, validated at load — and the `Animator` that walks one. |
| `gait` | `Gait`: the cycle phase driven by distance travelled, and the stride *measured* from a clip and rig so the planted foot does not skate. |
| `plant` | `Legs`/`Planted`: each foot solved onto its own terrain height, the root dropping to the lowest and leaning past the legs' reach. |
| `spring` | The one damped spring — solved in closed form, so no frame length can make it diverge — that recoil and sway are both built from. |
| `look` | `Look`: the head and spine turned toward a target, as a delta over whatever clip is playing. |
| `recoil` | `Recoil`: impulses on parameters springing back to the clip, the overshoot being the follow-through. |
| `sway` | `Sway`: cloth, hair and tails lagging the acceleration that carries them. |
| `breath` | `Breath`: the small always-on cycle that stops an idle figure reading as a paused one. |
| `shadow` | `Contact`/`Light`: the ground ellipse under the feet, solved through the projection rather than approximated. |

## An animation is authored in parameters, not rotations

A `Pose` is named scalars: how far an elbow is folded, how far a hip has
swung. Each is a fraction of that joint's *own* documented travel, scaled into
the interval by the rig's drive table, and three things follow.

One clip plays on any rig declaring the same parameters, because the clip
names no joint and no angle. An outward splay is handed once, in the drive
table, rather than in every clip that lifts an arm. And **no value of any
parameter can leave a limit**: a bent-backwards elbow is not a pose that gets
rejected, it is one that cannot be spelled — the elbow's parameter runs from
straight to fully folded and has no other end.

That guarantee is structural, and three rules hold it up. `Rigging::new`
refuses two drives on one joint axis, so no two parameters can sum past it.
No easing overshoots, so an interpolated value stays between its keys.
Blending is a weighted mean, so it stays between its inputs. A posture built
from a pose therefore has no out-of-limit case at all, and the only clamping
anywhere absorbs the last bit of floating-point rounding on a value already
mathematically inside.

Root motion is deliberately *not* a parameter. A jump's lift, the pelvis drop
of a crouch and a dodge's displacement cannot be decided without the ground
the feet are on, so they live with the terrain solve rather than split across
both — as the figure's whole-body root transform on the `Stance`, never as a
joint. The procedural layers keep the guarantee too: each states its effect as
a signed `Overlay` delta, the deltas sum, and the sum lands back inside the
parameter's range, so a layer crowded out by a pose already at its extreme
fades instead of fighting it.

## Bounds, and what they are not

`MAX_JOINTS`, `MAX_PARTS`, `MAX_RINGS`, `MAX_FITTED` and `MAX_STATES` bound
*authored content*, not a
runtime capacity: a rig is first-party code rather than input, and the shipped
rig's counts are held to them at build time. A figure wanting more joints than
this is a different figure, not a bigger one. They are also what keeps a
figure's buffers small enough to sit on a boot stack: a `Placement` is a
little over ten kibibytes and a `Rig` about the same. Runtime-loaded figure geometry
from an untrusted source is refused by the plan; only *parameters* are
validated data, and those are a later item.

Nothing here allocates. A `Rig` holds fixed arrays, and `Placement` is a
caller-held buffer sized to the largest figure, so drawing a scene of figures
costs no allocation at all.

## The layer order

One pass, and the order is the contract:

1. The `Animator` picks clips and cross-fades them; `Blend` resolves a `Pose`.
2. `Breath`, `Look` and `Recoil` each add an `Overlay`; the overlays sum and
   `Overlay::applied` brings the result back into range.
3. `Legs::plant` consumes that pose and answers the final one *plus* the root
   transform — it runs last because it must re-aim legs the layers above have
   finished with.
4. `Rigging::posture` and `Posture::place` draw it in the `Stance` the root
   transform went into. `Sway` turns the gear hung on its sockets, and the
   `Contact` shadow is painted first, under everything.

## Drawn as shaded strips

The visible half of each ring is split into four arcs, and each becomes one
closed strip down the part, filled at the tone its own surface normal takes
from the light. A cylinder then reads as a cylinder. The tone is rounded to a
fixed ladder, so the whole figure stays drawable in a small, exactly-known set
of colours — which is what lets the art harness check the palette by equality
and count separable regions rather than search for them.

Strips are stored in the scan converter's own sub-pixel units: half the
memory a pair of reals would take, and exactly what the painter hands it. The
cross-target digest folds the carried rings at full precision as well, so
nothing about the claim rests on the grid the drawing lands on.

## Measured, not asserted

Two numbers carry the parts that would otherwise be opinion. `Gait::fitted`
derives the stride from the clip and the rig — the foot's backward travel
through the body over the span it is on the ground — and `Gait::slide` reports
what is left, so "the feet do not skate" is a bound a test holds rather than
something a human squints at. `Planted::miss` reports how far a foot ended up
from the ground it was asked for, so a figure standing somewhere no figure
could stand is the simulation's defect to see rather than a fudged frame.

## What is not here

The species/build parameter space and the designer are FG6/FG7. The
contact-sheet harness that makes quality a measured property is
`cargo xtask artsheet`, which lives in `tools/xtask` because it renders and
writes files; the measurements it gates on live here, in `quality` and
`paint`, so they run on every Tier-1 target under `cargo test`.

Clips and machines are *code* here, assembled from borrowed static tables and
checked once. Loading either from an untrusted source is a later item and
will validate at its own boundary; nothing in this crate parses input.
