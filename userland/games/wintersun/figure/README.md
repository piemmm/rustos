# tairix-wintersun-figure

WinterSun's figure engine: the rig a character exists as before anything
animates it — a parent-relative joint hierarchy with per-axis rotation limits,
parts bound to joints, named equipment sockets, the depth-sorted draw order,
and the one body frame that serves every heading (`plans/FIGURE.md` FG2).
Stability tier: **experimental** — nothing has shipped, so a type changes in
place until it does.

`no_std`, no allocator, `forbid(unsafe_code)`.

## Why a rig and not a sprite sheet

Eight headings × every clip × every equipment combination is artwork nobody
can author or keep consistent, and a sheet is unreviewable by a test: a
regression in it is invisible until a human looks. So a figure is parametric
parts on a skeleton, drawn through `lib/raster`'s one anti-aliased scan
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

`yaw` never turns an outline at any heading: a ground-plane rotation projects
to a shear, and every `lib/raster` outline is symmetric about its own vertical
axis, so the shear leaves it be. That symmetry rule is what makes the free
turnaround possible — an asymmetric outline would need mirroring, and
mirroring is the per-direction branch the body frame exists to avoid.

## Modules

| Module | What it owns |
|---|---|
| `frame` | The body frame: `Body`, `Rotation`, the orthonormal `Basis` and its composition, the projection, and the screen turn a joint's rotation shows. |
| `joint` | `JointId`, the checked `Limit`/`Limits` intervals, and `Joint` — a parent, a rest transform, and how far it turns from it. |
| `socket` | The closed socket set equipment hangs on, and where a rig mounts each one. |
| `rig` | `Rig` and its validation, `Posture`, `Part`/`Fitted`, and the `Placement` a figure is projected and depth-sorted into. |
| `humanoid` | The first-party humanoid: 17 joints, 21 parts, every socket, proportioned in percentages of `STANDING_HEIGHT`, and the drive table binding the pose parameters to them. |
| `pose` | `Param` — the 22 named scalars an animation is authored in — the `Pose` holding them, their `Range`, and the `Mask` a blend writes through. |
| `rigging` | `Drive`/`Rigging`: which joint axis a parameter turns, and which way its `+1` points. |
| `clip` | `Clip`: a keyed `Curve` per parameter with an `Easing` per segment, a duration, a `Loop` mode, and the named `Event`s at phases along it. |
| `blend` | `Blend`: weighted accumulation of poses and clips, weighed per parameter so a mask means something. |
| `transition` | `Transitions` — states, clips and per-edge cross-fades, validated at load — and the `Animator` that walks one. |

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
the feet are on, so they live with the terrain solve in FG4 rather than split
across both.

## Bounds, and what they are not

`MAX_JOINTS`, `MAX_PARTS`, `MAX_FITTED` and `MAX_STATES` bound *authored
content*, not a
runtime capacity: a rig is first-party code rather than input, and the shipped
rig's counts are held to them at build time. A figure wanting more joints than
this is a different figure, not a bigger one. Runtime-loaded figure geometry
from an untrusted source is refused by the plan; only *parameters* are
validated data, and those are a later item.

Nothing here allocates. A `Rig` holds fixed arrays, and `Placement` is a
caller-held buffer sized to the largest figure, so drawing a scene of figures
costs no allocation at all.

## What is not here

The procedural layers over a clip are FG4: gait phase driven by distance
travelled rather than a timer, look-at, recoil and follow-through, cloth and
hair sway, breathing, each foot planted on its own terrain height, root
motion, and the contact shadow. The contact-sheet art harness that makes
quality a measured property is FG5; the species/build parameter space and the
designer are FG6/FG7.

Clips and machines are *code* here, assembled from borrowed static tables and
checked once. Loading either from an untrusted source is a later item and
will validate at its own boundary; nothing in this crate parses input.
