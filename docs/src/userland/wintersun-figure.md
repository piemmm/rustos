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

## What comes next

Pose parameters, keyframed clips, blending and the transition machine; then
the procedural layers over them — gait phase driven by distance travelled
rather than a timer, look-at, recoil, cloth sway, breathing, and feet planted
on real slopes; then the contact-sheet harness that makes readability,
palette conformance, joint limits, foot slide, motion continuity and loop
closure measured, gated properties rather than opinions.
