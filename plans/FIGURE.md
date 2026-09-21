# FIGURE.md — parametric figures: shared primitives, the game rig, and provable art quality

Binding under `AGENTS.md`. This plan owns **how a character exists**: the
parametric shapes a figure is drawn from, the skeleton that places them, the
pose parameters and clips that move them, and — the part that matters most —
the harness that makes "the art is good" a **measured, gated property** instead
of an opinion.

It is deliberately split across two homes, because exactly one part of it is
general and the rest is a game's:

| Half | Home | Why there |
|---|---|---|
| The parametric **outline primitives** — a superellipse, a taper, a wedge, a scalloped panel, a bevelled panel, a splat — and their tracer | `lib/raster::shape` | Two independent consumers (`cinder` and the game) and no game semantics: they are 2D vector primitives feeding the scan converter `lib/raster` already owns, sitting beside `fill_round_rect`. |
| **Everything with character semantics** — rigs, joint limits, equipment sockets, draw order, the body frame, pose parameters, clips, blending, the transition machine, motion layers, character parameter spaces, presets, the art harness | `userland/games/wintersun/figure` | A rig is game content, not OS infrastructure. `lib/*` is the OS's shared-library namespace and a figure engine has no business in it. |

That line is the whole organisational decision, and it is drawn at "does this
carry meaning about a *creature*?". A taper is geometry; a *limb* is anatomy.
The primitive is shared under its geometric name and the anatomy stays with the
consumer that means it — which is also why the shared names are `Taper` and
`Splat` rather than `Limb` and `Fur`.

A figure's own body is **not** drawn from those primitives — §2 says why, and
the short version is that a flat outline cannot be placed correctly under a
three-axis rotation. `lib/raster::shape` stays shared for what it is good at:
`cinder`'s whole cat, and the ground ellipse a figure's contact shadow is.

`cinder` therefore migrates only its **shape** code (FG1) and keeps its own
skeleton, gait, pose model and mind exactly where they are. It never depends on
the game: `userland/games/*` is a leaf subtree nothing outside may depend on
(§17.4), so that dependency could not exist even if someone tried.

Read first (§15.18): `plans/CINDER.md` §B2 (the proven shape vocabulary and
skeleton this generalises), `AGENTS.md` §10 (asset tiers and DPI), §27
(foundational primitives are complete, not minimal), `plans/WINTERSUN.md` §1
(the games subtree) and §4, `plans/GUI-CONTROLS-DESIGN.md` (the designer's
controls), `lib/raster` and `lib/util::mathf` rustdoc.

## Ledger

| # | Item | Status |
|---|---|---|
| FG0 | This plan, the jump-sheet row, the §3 map entry, the `PLAN.md` section | done |
| FG1 | `lib/raster::shape`: the six outline primitives, the tracer, the build-time vertex bounds, and `cinder` migrated onto them with its existing tests as the acceptance gate | done |
| FG2 | `wintersun/figure`: the rig — skeleton, joint hierarchy with limits, named equipment sockets, draw order, the one body frame that serves every heading, and the skinned meshes a part is drawn as | done |
| FG3 | Pose parameters, clips (keyframed parameter curves with easing), clip blending, and the transition state machine | done |
| FG4 | Procedural layers over a clip: gait phase from distance travelled, look-at, recoil, cloth and hair sway, breathing, per-foot terrain planting, root motion and the figure's root placement, contact shadow | done |
| FG5 | `cargo xtask artsheet`: the shipped motion set, the painter, the contact-sheet renderer, the committed ledger, and the automated quality checks | done |
| FG6 | The parameter space: species and build parameters, the palette model, validated bounds, and the compact serialised form a character record stores | planned |
| FG7 | The designer engine: the parameter model, live preview, presets, and randomised-but-plausible generation | planned |

Items are built in ledger order; each is complete — tests, docs, green gate —
before the next begins.

## 0. The problem this plan actually solves

Hand-drawn sprite sheets are how this kind of game is normally made, and they
are the wrong answer here for reasons that are structural rather than a matter
of taste:

- **Eight headings × N clips × M frames × every equipment combination** is a
  combinatorial explosion of artwork that no one can author or keep consistent,
  and equipment then cannot be mixed at all without redrawing it.
- **A sprite sheet is unreviewable by a test.** A regression in it is invisible
  until a human looks, so it rots silently.
- **It does not scale with DPI.** §10 requires every desktop length to resolve
  through one scale factor; a fixed-resolution sprite either blurs or aliases.
- **It cannot be generated honestly by an AI contributor**, which is the
  practical point. Asked for a 16-frame run cycle as pixels, a language model
  produces something that looks approximately right and is wrong in exactly the
  ways that matter — sliding feet, popping joints, inconsistent silhouettes,
  drifting palettes. Committing that and calling it art is the failure mode
  this plan is written to make impossible.

The alternative already works in this tree. `cinder.app` draws a cat from
**sixty-four parametric parts on a skeleton**, with pose parameters as data, one
body frame so a single rig serves every heading, worst-case vertex counts
asserted at build time, and host tests over the shapes, the gait, and the
painter. It is legible, it is scalable, it is reviewable, and every property
that can be stated as a number is stated as one.

So: generalise it, and then make quality measurable.

## 1. FG1 — the shared primitives, and `cinder`'s migration

`lib/raster::shape` owns the primitives. The set generalises
`cinder/src/shape.rs` without acquiring any game meaning:

| Primitive | The geometry | What a consumer uses it for |
|---|---|---|
| `Superellipse { rx, ry, square }` | an ellipse pulled `square` of the way to its bounding box — corners round while flats stay flat | trunks, skulls, haunches, eyes |
| `Taper { length, top, foot }` | a taper from an origin to a rounded end, rotated about that origin | arms, legs, tails, weapon shafts |
| `Wedge { half_width, height, lean }` | a leaning wedge with a tucked base | ears, horns, blade tips, spurs |
| `ScallopedPanel { rx, ry, folds }` | a panel with a scalloped hem | cloaks, skirts, banners, tabards |
| `BevelledPanel { rx, ry, bevel }` | a hard-edged panel with a bevelled rim | armour, shields, buckles, metal |
| `Splat { radius }` | a radial blob with a deterministic angular ripple | fur, hair, foliage, smoke |

Every name states the *geometry*; the third column is the consumer's business
and is documentation, not API. That is the rule that keeps this from being
disguised game content in `lib/*`: `cinder`'s cat calls a `Taper` a leg in its
own rig table, and `lib/raster` never learns that legs exist.

`Wedge` is `cinder`'s `Ear` renamed to state what it is rather than where it
was first used. `BevelledPanel` is the one genuinely new primitive, because
armour drawn as a superellipse reads as flesh. Anything a primitive cannot
state is a *composition* of primitives, never a seventh variant added for one
asset (§2.3).

`Splat` is the judgement call in the set, and worth naming as one: it was
written for cat fur. It is kept because a radial blob with deterministic edge
irregularity is equally foliage, smoke, a blot or a starburst — general
geometry that happens to have had a creature as its first caller. If a reviewer
disagrees, the alternative is for both consumers to carry it privately, which
is the duplication §2.2 forbids; it is not to keep it in `lib/*` under a
creature's name.

Every outline is filled through `lib/raster`'s single anti-aliased scan
converter — no second rasteriser (§2.2) — into `lib/inline` fixed arrays, so
the outline path touches no allocator. Each shape's worst-case vertex count is
asserted against the buffer at **build** time, so a generator given more detail
fails the build rather than silently truncating a ring into a shape nobody
authored. That rule is `cinder`'s and it is kept.

**`cinder` migrated in the same change**, so no private copy survives beside
the shared one. Only its `shape.rs` moved: its skeleton, gait, roam and mind
stayed where they are, because those are one creature's content. Its shape
tests moved with the code and now cover the module in `lib/raster`; its
remaining 146 tests and the desktop-companion QEMU vertical passed unchanged,
which is what says the pixels are unchanged. `cinder` uses five of the six —
`BevelledPanel` has no consumer there — and `Splat` still carries no outline,
because the soft composite that draws it is `cinder`'s `fur`, not a shared
primitive.

What the module now guarantees: outlines in shape-local pixels with `y` up,
every one symmetric about its local vertical axis bar the leaning `Wedge`,
traced into `lib/inline` fixed arrays whose worst case is asserted against
`MAX_VERTICES` at build time, and filled through the one anti-aliased scan
converter. A caller that asks for more detail than the buffer holds fails the
build rather than drawing a truncated ring.

## 2. FG2 — the rig (game-side)

A `Rig` in `wintersun/figure` is a skeleton: joints in a parent-relative hierarchy, each with a rest
transform and **documented rotation limits**; parts bound to joints with a
body-local offset; a draw order that is the skeleton's own, so a piece of
piping stays behind the flap it edges; and named **sockets** where equipment
attaches (hand, off-hand, head, back, shoulder, hip, foot).

Two rules carry most of the visual quality:

- **A joint that bears a limb also carries its mass.** Each limb's joint is
  also where the trunk carries a shoulder or haunch, from one table, so a
  swinging shank can never open a gap at the shoulder. Legs that floated below
  the body were the defect this closes in `cinder`, and it generalises exactly.
- **One body frame serves every heading.** A figure is not drawn from
  per-direction sprite sets. Its parts are placed in a body frame that is
  rotated by the heading and whose depth axis is foreshortened by the figure's
  own drawing elevation, and they paint far-first by projected depth — so
  eight or sixteen headings need no new artwork and no `cfg`. This is
  `cinder`'s insight and it is the single largest saving in the whole design.
  A figure's **size does not vary with depth**: the world projection is
  orthographic (`wintersun/app`'s `Camera`), so a figure whose scale tracked
  its ground row would grow and shrink as the camera scrolled. The
  foreshortening applies to the figure's own frame, not to its size.

Equipment is parts on sockets with their own palette, so gear is visible,
mixable, and costs no new art path. A helm is a `Plate` and a `Wedge`, not a
redrawn head.

### A part is a skinned mesh, not a billboard

The rig first drew each part as one of the §1 outlines, placed from an
origin, a screen angle and the bone's own length. That cannot work, and FG5's
harness measured how badly: against the shipped walk a thigh's drawn end
missed the knee it hangs from **by up to a third of the figure's height**.

```
facing east:  thigh drawn end (-12.07,-37.72)   knee at (+12.07,-37.72)
facing south: thigh drawn end (  9.00,-28.00)   knee at (  9.00,-22.97)
```

Two independent defects, and neither is a bug to be fixed in place. The
screen angle was a projection of a three-axis rotation vector onto the one
axis a billboard can turn about, and its *sense* was inverted — every swung
limb was drawn swinging the wrong way at every heading but the degenerate
ones. And the outline was drawn at the bone's authored length while the
projection shortens a limb pointing into the scene by twenty to thirty per
cent. A flat outline placed from three numbers has no way to end where its
child joint is.

So a part is a **mesh**: a run of cross-section rings along a spine, each
ring carried by the joint chain in three dimensions and then projected
vertex by vertex. There is no placement left to get wrong — a limb's far end
*is* its child joint, at whatever length and angle the heading leaves it,
exactly, everywhere.

- **Skinned, so a joint bends rather than creases.** A ring states how far
  it is carried by the part's *end* joint rather than its own. The last ring
  of a spanning part is carried wholly by the far joint, which is what puts
  the surface's end exactly where that joint is; the rings before it take
  the blend, which is what makes the bend smooth. A gated test holds every
  seam rigid across eight headings and six poses.
- **Drawn as shaded strips, so a limb reads as round.** The visible half of
  each ring is split into four arcs and each becomes one closed strip down
  the part, filled at the tone its own surface normal takes from the light.
  The tone is rounded to a fixed ladder, so the whole figure stays drawable
  in a small, exactly-known set of colours — which is what lets the harness
  check the palette by equality and count separable regions at all.
- **No new rasteriser and no depth buffer.** A strip is a closed contour
  filled through `lib/raster`'s existing scan converter, and parts are
  depth-sorted exactly as before. The shipped figure traces 688 outline
  points against the billboard's 468, so it is not the dearer design.
- **Still no allocator.** A part holds its rings in a fixed array and a
  placement holds its strips in another; nothing on the path allocates.
- **Every free end is closed.** An open tube shows its own near rim as a
  crescent where the surface should have ended — at the crown of a head that
  reads as a notch cut out of it, and at a shoulder as a wing. Capping is
  data, not code: the first and last rings of an exposed end taper to a
  point.

What the built rig guarantees: the hierarchy is a parents-first forest, so no
cycle can be spelled and a posture resolves in one forward pass; a joint that
bears a child carries a part of its own, and no child's origin lies beyond
everything its parent draws (one-sided — a part's reach is an outer bound, so
exceeding it proves a gap while clearing it does not prove a seam, which the
mesh now makes structural rather than measured); a part that spans a bend
names a far joint that is genuinely its own joint's child, so the rest
transform between them exists; and a `Posture` refuses a rotation outside its
joint's limits, so an out-of-limit pose fails where it is authored rather than
on the frame. Rotations are right-handed about the body axes with no sign
flipped, so a part hanging below its joint turns opposite to the joint's own
`forward` — which makes an outward splay a different sign on each side, and
the humanoid's shoulder and hip roll limits handed. The shipped humanoid is 17
joints and 21 skinned parts authored in percentages of its standing height,
with both sides mirrored from one pass. Nothing in the crate allocates.

## 3. FG3/FG4 — animation

**Pose parameters are data, not code paths.** A `Pose` is a closed set of
named scalars — spine bend, twist and tilt, head turn, nod and tilt, and per
side the shoulder swing and splay, elbow bend, wrist angle, hip swing and
splay, knee bend and ankle angle — and a figure is drawn from one pose. A
`Clip` is a keyframed curve per parameter with an easing per segment, a
duration, and a loop mode. Blending is a weighted sum of poses with per-clip
masks, so a cast animation can play on the upper body while the legs keep
walking.

**A parameter is a fraction of the joint's own travel, which is what makes a
clip rig-independent and an illegal pose unspellable.** The rig's `Drive`
table says which joint axis a parameter turns and which way its `+1` points,
so the angle is scaled into that axis's documented interval. A clip names no
joint and no angle and therefore plays on any rig declaring the same
parameters; the handedness of an outward splay is stated once in that table
rather than in every clip that lifts an arm; and a bent-backwards elbow is
not a pose that gets rejected but one that cannot be written down, because
the elbow's parameter runs from straight to fully folded and has no other
end. Three rules keep that structural rather than checked: `Rigging::new`
refuses two drives on one joint axis so no two parameters can sum past it, no
easing overshoots so an interpolated value stays between its keys, and a
blend is a weighted mean so it stays between its inputs. The only clamping
anywhere absorbs floating-point rounding on a value already mathematically
inside. Overshoot is not an omission: the snap of a recoil and the settle of
a follow-through are damped layers below, where they can be bounded on their
own terms rather than smuggled into a keyframe.

**A pose is articulation only; everything that moves the root is FG4.** A
jump's lift, the pelvis drop of a crouch and a dodge's displacement are
translations of the figure's root, and none can be decided without the ground
the feet are standing on. They therefore sit with the foot-planting and
root-motion layers below rather than being split across both items, and a
crouch is authored as hip, knee and ankle bend whose ground contact FG4
resolves. The pelvis is left undriven by any parameter for the same reason:
it is the root, and turning it tilts the whole figure, which is the slope
response FG4 owns.

**The root transform is the placement's, not the pelvis joint's.** FG4 carries
the figure's root as a rigid transform of the whole body — a `Body` offset in
figure-local units and a `Rotation` — held on the `Stance` that `Posture::place`
takes, and seeded into the resolve as the frame the parentless joints hang in.
Two measurements decide it against the alternative of rolling the pelvis joint.
The pelvis's roll limit is +/-0.20 rad, which across the humanoid's 18-unit
stance absorbs 3.58 units of height difference, a slope of 11.2 degrees; the
planting reach the tilt exists to back up is the leg's own span travel, 29.9
units, a slope of 59.0 degrees. A fallback that saturates five times earlier
than the thing it backs up is no fallback. Independently, a pelvis roll pivots
about the pelvis and swings the feet sideways through the ground, where a
whole-figure tilt must pivot about the ground contact — which is the body
frame's origin and so the placement's, not any joint's. Offsetting the surface
ground point instead was rejected for a third reason: it is a second, unscaled
convention beside the figure-local units every other authored length uses, and
it drops the depth component a root displacement has.

**Blending weighs each parameter, not each pose.** A parameter is weighed
only against the clips that had an opinion about it, which is what makes a
mask mean anything, and a parameter nothing wrote resolves to rest. That
fixes how partial animations compose: an overlay covering part of the body is
layered *over* a base covering the rest — added to the same blend — rather
than cross-faded against it, since cross-fading a full-body clip out from
under a partial one would leave the uncovered half at rest as the base's
weight reached zero.

A `Transitions` state machine selects clips from the simulation's state
(grounded, speed, action, stagger) with per-edge blend durations. The machine
is data and is validated at load: every state reachable, every clip referenced
present, every blend duration positive.

**A clip also carries named events at phases** — `footstep`, `hit_frame`,
`loose`, `cast_release` — which is the seam the game's combat timing is built
on (`plans/WINTERSUN.md` §5, "Game feel"): a hitbox opens and a sound plays on
the frame the art shows it, rather than on a timer that drifts from the
animation. The events are part of the clip because that is the only place the
phase is known; what a consumer *does* with `hit_frame` is the consumer's
business, and the engine never learns what a hitbox is. An event at a phase
outside `0..=1`, or a clip declaring an event name twice, is refused at load.

Over the clip sit **procedural layers**, which is where animation stops looking
keyframed:

- **Gait phase is driven by distance travelled, not by a timer.** Feet are
  planted as a function of position, so a walk cannot slide, cannot skate when
  the speed changes, and cannot walk on the spot. `cinder`'s `roam` already
  proves this is the difference between motion and the appearance of it.
- **Look-at** rotates head and eyes toward a target within the joint limits.
- **Recoil and follow-through** displace the rig briefly on an impact or a
  loose, and settle on a damped curve — the thing whose absence makes an
  attack feel weightless.
- **Cloth, hair, and tail** are damped springs driven by acceleration and the
  wind vector, so a cloak trails the turn instead of rotating with it.
- **Breathing** is a small always-on cycle, which is what stops an idle figure
  reading as a paused one.
- **Feet are planted on the ground, not on the ground's average.** The world
  generator produces real slopes, so a figure standing across a gradient has
  one foot higher than the other; without correction both feet sit at the
  root's height and visibly float on the uphill side and sink on the
  downhill. Each foot is therefore solved to its own terrain height within a
  stated reach, and the excess is absorbed up the chain — the pelvis drops
  toward the lower foot and the supporting knee takes the bend, within the
  joint limits FG2 declares. A slope steeper than the reach allows tilts the
  whole figure rather than tearing the rig. **This is a correctness
  requirement, not polish**: a top-down camera looks straight at the
  ground-contact line, which is exactly where the error is most visible.
- **Root motion where a clip needs it.** A dodge, a lunge and a stagger
  displace the figure by an amount the *clip* owns, so the animation and the
  movement cannot disagree. The authoritative displacement stays the
  simulation's (a client cannot move itself by playing an animation); the clip
  supplies the curve the simulation's own move follows.
- **A contact shadow** at the ground point, squashed by the light direction and
  fading with height — the trick that makes a jump readable.

**The height a foot is asked for is the clip's own, not the ground.** Two
defects the art harness surfaced, both in how the planting solve reads a pose.
Putting *both* feet on the terrain flattened a walk's swing arc into a
shuffle; and because the pelvis sits at the rig's own fixed height, a foot
cannot travel fore and aft along level ground without the whole figure
sinking — so a walk authored with folded legs was left hovering over the floor
by exactly that fold. Both follow from one reading of the pose: a leg reaches
no further than straight, so the height a clip puts an ankle at is always at
or above a straight leg's, and the difference is how far that leg is folded.
The *smaller* of the two is the crouch the figure stands in and the root sinks
by it; what is left over is a foot the clip lifted, and it keeps that
clearance over whatever terrain it lands on. On flat ground the articulation
therefore comes back untouched for *any* pose, with the planted foot on the
floor.

**Still open: a figure with neither foot down.** During a run's flight phase
the rule above has no planted foot to read, so it takes the lesser lift and
the figure dips where it should rise — measured at one unit in a hundred over
a quarter of the shipped run's cycle. The honest fix is a clip-authored root
height, which crosses the FG3 line that a pose is articulation only; it is
recorded here rather than papered over, and is the next thing FG4 owes.

Every layer is a pure function of (pose, state, time) and is host-tested
against its stated property, not against a screenshot.

## 4. FG5 — making quality provable

This is the heart of the plan, and it is done. Three mechanisms, and none of
them is optional.

### The golden is a committed ledger; the sheets are on-demand output

`cargo xtask artsheet` walks the shared reference grid — every shipped motion,
at eight phases, facing four ways, at the three pixel sides the desktop draws
a figure at — renders each cell, measures it, and holds every number against a
bound. `--write` regenerates, the bare form verifies and fails closed, and it
runs in `ci` beside `font-atlas`.

**The committed golden is text, not a picture.** A committed PNG reproduces
§0's own objection one layer up: a reviewer cannot read a binary diff, so a
regression stays invisible until somebody opens the file, and git carries the
churn on every rig, clip, shape or palette change.
`userland/games/wintersun/figure/artsheet.ledger` is one row per cell —
identity, a pixel digest, and every measured number — so a change reads as
`skate 0.002718 -> 0.014803` in the diff. `--sheets` renders the pictures on
demand into the gitignored `images/artsheet/`, which is what makes the thing a
human judges always current rather than as-of-last-regeneration.

The bare form does **both** halves: it regenerates the ledger and compares it
byte for byte, *and* it checks the freshly measured numbers against their
bounds. Drift alone would admit a regression somebody had regenerated; bounds
alone would admit a change nobody noticed.

### Automated quality checks, per cell

Each measurement has a bound beside it — the pose-side ones in
`figure::quality`, the pixel-side ones in the harness — and a bound is never
widened to admit a change.

- **Joint limits.** Every sampled pose is turned into a `Posture`, which
  refuses an out-of-limit rotation, and the excursion is read back off the
  rotations the posture actually holds. The bound is below one on purpose: a
  clip pinned at a limit reads as a rig fighting itself and leaves the overlay
  layers nowhere to go. Shipped worst: 0.79.
- **Foot slide.** Consumed from `Gait::fitted`/`Gait::slide` and divided by the
  stride, so the bound is dimensionless. Shipped worst: 0.009 of a stride,
  about one pixel over a whole cycle at the largest drawn size.
- **Motion continuity.** The largest second difference of any parameter across
  a cycle, per unit of that parameter's range, taken cyclically for a clip that
  joins. It separates a pop from a keyed curve's own faceting rather than
  measuring how finely the curve was keyed. Shipped worst: 0.041.
- **Loop closure.** How far a looping clip's last pose sits from its first. The
  shipped tables are authored to join exactly, so the bound is rounding.
- **Silhouette readability**, three measured numbers per cell: the
  alpha-weighted **coverage ratio** inside a band; the count of **connected
  tonal regions**; and the **contrast ratio** against both themes' desktops.
- **Budget.** Outline points per figure and fill area per cell, so a rig
  cannot quietly become the frame's cost centre.

### The honest limit

The readability check has a second consumer: `plans/WINTERSUN.md` §3 fixes the
renderer's degradation floor as the last detail level whose frames still clear
these bounds. That consumption — reading the ledger into `app::quality::Ladder`
— is WS6's change, because there is no detail ladder in the figure crate to
floor. So a change that loosens these numbers does not merely admit a worse
contact sheet; it lets the running game shed detail past the point a player can
read it.

### What FG5 settled, and where it diverges from this plan as written

- **A contact sheet is output, not a golden** (above). Recorded because the
  original text said "PNG contact sheets committed to the tree".
- **Palette conformance is checked source-side.** Every strip's colour is held
  against the rig's own declared tones at the painter's own shading steps —
  exact, cheap, and stronger than a pixel check, which would need an
  antialiasing tolerance that could hide real drift. The *pixel* side classifies
  each pixel by its nearest declared shade, which is what makes the region count
  a count of separable masses rather than a colour search.
- **The shipped motions carry no `clip::Travel`.** The original text said walk
  and run would. They must not: a walk's displacement is the simulation's own
  and the *gait* paces it, so a travel curve would be a second pacing of one
  thing, and for a constant-speed cycle it is the identity ramp. `Travel` is for
  a move the animation paces — a dodge, a lunge, a stagger — and those are
  WinterSun content.
- **The shipped motion set is three clips**, authored here because §4 requires
  the harness to sheet "every clip" and every walk in the tree was a test
  fixture. Idle, walk and run are the honest minimum that exercises every check.
  Their leg curves are not authored by eye: each states a **foot path** — strike
  distance, crouch depth, swing clearance, stance fraction — solved through the
  same two-bone geometry the planting layer uses, and a test measures the stride
  back out of the keys and checks it against the number the path was authored to
  give.
- **The figure gained a fourth cross-target digest.** `figure::digest` folds the
  complete placed-strip stream, the planting roots and misses, the gait's own
  phases and the quality numbers, over raw `f64::to_bits` with no quantisation.
  One vertical per Tier-1 target, matching `world_determinism` and
  `rules_determinism`.
- **The accessor audit ran, and kept three the letter of it would have cut.**
  Deleted for having no caller at all: `Clip::events`, `Travel::keys`,
  `Spring::damping`, `Recoil::spring`, `Contact::radius`, `Light::elevation`,
  and all four `Stance` readers — plus `frame::screen_turn`, which the mesh
  left with nothing to do. Kept where the only caller is a test that
  genuinely needs it: `Curve::keys` (the motion tests check the authored
  tables are evenly spaced, which is what the half-turn mirror depends on,
  and the alternative is enumerating the tables by name), `Breath::depth` and
  `Spring::rate` (both bound an assertion against the value the object was
  built with, rather than restating it). An accessor whose only caller is a
  *tautology* test went; one whose caller is a real assertion stayed.
- **The harness found two structural defects in FG2's placement and one in
  FG4's planting**, all fixed: the billboard could not place a limb (§2), and
  the planting solve both flattened a walk's swing foot and left a crouching
  figure hovering over the ground (§3).

## 5. FG6/FG7 — the parameter space and the designer

A figure's identity is a validated parameter record: species, build (height,
mass distribution, limb proportion, head proportion), features (face shape, eye
shape and colour, ear form, horn or tail presence and form, hair style and
volume), and a palette (skin/fur, hair, eyes, markings, cloth accent). Every
parameter has documented bounds, and the record's decoder is total and fails
closed — because in `plans/WINTERSUN.md` it arrives **off the wire from a
client**, which is assumed hostile: the server re-validates every parameter
against its bounds and refuses an impossible figure rather than drawing one.
The serialised form is compact and versioned, since a character record stores
one per character.

The designer engine is the parameter model plus the preview; the surfaces are
the game's (`plans/WINTERSUN.md` WS17). Two obligations bind it:

- **§28, which a designer is the surface most likely to violate.** A slider
  changes the parameter in memory and repaints — it does not write a store, and
  it does not re-derive anything the parameter does not feed. The durable write
  happens once, when the drag settles. The known real-world defect this cites
  is the settings slider that wrote to the configuration service on every
  pointer-motion sample and froze its window for the whole drag, and then, with
  the write removed, still re-derived the entire surface per sample.
- **Randomised means plausible, not uniform.** "Surprise me" draws from
  per-parameter distributions with correlations (a heavy build gets broader
  shoulders; a pale palette gets pale markings), from an injected
  `lib/rng` generator so it is deterministic and host-testable. Uniform
  sampling over a parameter box produces monsters, which is how a designer
  earns a reputation for ugly output.

## 6. Refused by name

- **A figure engine in `lib/*`.** Rigs, clips and character parameters are game
  content; `lib/*` is the OS's shared-library namespace. Only the geometric
  primitives are shared, and only under geometric names (§1).
- **Hand-drawn per-direction sprite sheets**, for the reasons in §0.
- **A seventh shape variant added for one asset.** Compose from the six.
- **A second rasteriser, blend, or outline path.** `lib/raster` owns them.
- **Runtime-loaded figure geometry from an untrusted source.** Parameters are
  validated data; geometry is first-party code.
- **Screenshot-diff tests as the only animation check.** They catch that
  something changed, never that it is wrong. The measurements in §4 are what
  state correctness; the sheets are for the human judgement that remains.
- **An `#[allow]` or a widened bound to make a quality check pass.** A failing
  readability, slide, or limit check is a real finding (§15.3, §2.18).

## 7. Verification

- The six shapes' outlines are traced within their asserted vertex bounds, and
  a generator exceeding one fails the build.
- `cinder`'s existing shape, paint, gait, roam, and vertical tests pass after
  the FG1 migration, with unchanged pixels where the shape is unchanged.
- Rig: joint limits enforced at the posture, so an out-of-limit rotation is
  refused where it is authored; a bearing joint without a part of its own, and
  a child beyond its parent's reach, are both refused at assembly; a limb's
  joint always carries its mass, measured by placing a posed figure and
  finding the cap and the limb at one point; draw order is the skeleton's, and
  reverses on the turnaround rather than needing a second set of parts; every
  socket resolves, and gear naming an unoffered one is refused rather than
  dropped. The projection's properties are numbers — the foreshortening is the
  elevation its doc claims, height is unforeshortened, depth is not the screen
  row, and the camera direction moves nothing on screen — not screenshots.
  Meshes: a carried ring sits where its joint puts it with its cross-section
  square to the surface; a ring bound to the far joint lands exactly where
  that joint does and one bound partway moves partway; the near arc is the
  half facing the camera and no point of it faces away; a flattened ring's
  normals come out of its flat side; the shading ladder is a closed, climbing
  set that a step past its end lands on rather than past. And the property the
  whole mesh exists for: two surfaces meeting at a joint are **rigidly**
  joined, over eight headings and six poses, to within rounding. `no_std` with
  no allocator, built on all four Tier-1 targets.
- Clips: curve evaluation at known keys and midpoints; loop closure, so a
  wrapping clip reads the same value either side of the join; no easing
  leaves the unit interval or goes backwards; a sampled value never leaves
  its parameter's range under any loop mode. A cross-fade's two weights sum
  to one, and a blend of values inside their ranges stays inside them — the
  general guarantee, since weight is normalised per parameter rather than
  required to sum to one. Malformed clips and machines are refused at load:
  an empty or non-ascending curve, a key or event outside `0..=1`, a keyed
  value outside its parameter's range, two curves on one parameter, a
  duplicated event name, a non-positive duration or cross-fade, a repeated or
  out-of-order edge, a state naming an absent clip, and a state nothing leads
  to. Events fire exactly once as the phase passes them, report a lap's tail
  before the next head in the order they happen, and survive a blended
  transition without duplicating or being dropped.
- Rigging: two drives on one joint axis are refused, so every parameter at
  either extreme — singly and all at once — leaves every joint of the shipped
  humanoid inside its limits; an elbow cannot be driven past straight at any
  value; an outward splay is outward on both sides and equal in magnitude;
  each half of a lopsided limit is scaled on its own, so rest stays rest.
  `no_std` with no allocator, built on all four Tier-1 targets.
- Layers: the gait phase follows distance rather than frames, so the same
  ground covered gives the same phase however it was divided and a figure
  held still does not walk on the spot; a known distance completes a known
  number of cycles, forward and backward, and the phase never leaves the
  half-open cycle. The stride is *measured* — fitted from the clip and rig,
  it recovers the one the test's walk was authored with to within a
  twentieth, and the planted foot then slides under a hundredth of a stride;
  a stride that is not the clip's own slides at least ten times further,
  which is what makes the fitting earn its answer. A clip whose foot never
  lifts has no stride and is refused rather than given an invented one, and
  a walk authored from mid-stance measures the same as one from the head of
  the cycle.
- Planting: on flat ground the solve is an **identity** — every parameter
  unchanged and no root at all — so a figure on the level is drawn exactly as
  its clip authored it; off it, each foot lands on its own terrain height to
  within a ten-thousandth, verified by resolving the solved pose rather than
  by trusting the solver's own arithmetic. The root drops to the lowest foot
  and never lifts; a slope inside the reach leaves the figure square and one
  past it leans, handed by which foot is higher; ground no leg can reach is
  reported as a miss rather than fudged. Every solved pose stays inside its
  parameter ranges and is posturable, across both a rest and a striding pose
  and the whole span of slopes. Reach and stance come from the rig's own
  joint table, and a leg whose joints are not a chain is refused.
- Root placement: a lift moves every part by exactly itself and a tilt turns
  the figure about its ground contact rather than flinging a part outward;
  an unreal scale, anchor, offset or tilt is refused where the stance is
  built rather than where it is drawn.
- Springs: no step of any length, over six damping ratios from undamped to
  heavily overdamped, gains amplitude or leaves the numbers — the closed
  form is path-independent, so splitting a step in two gives the same answer
  as taking it whole. Every regime settles on its target; only an
  underdamped one overshoots, which is the follow-through. A recoil moves
  nothing until time passes, touches only what it struck, overshoots on the
  way back, and settles onto the clip. A sway hangs straight under steady
  motion, leans against an acceleration and with a wind, cannot be driven
  past its limit by any input, and comes back to rest after a thirty-second
  frame.
- Look-at: a target straight ahead moves nothing and one on the head itself
  is left alone; a reachable target is looked straight at; every target
  leaves the head nearer to it than it started; one out of reach stops at
  the limit with the pose still posturable; and the spine share moves work
  between spine and neck without changing the total turn.
- Root motion: the curve runs from none of the move to all of it or is
  refused, values outside that are refused, and the distance is the
  simulation's — so a move delivers exactly what was authorised however the
  clip paced it, including an anticipation that draws back first.
- Contact shadow: an overhead light lays the footprint flat and
  foreshortened; the solved screen ellipse matches the ground ellipse swept
  and projected the long way round, at every bearing; a lower light rakes it
  out along its own bearing only, and one near the horizon stops raking
  rather than running away; a rising figure's shadow slides away along the
  light and thins, and a figure below its ground point casts as if on it.
- Breathing is non-zero at idle, never exceeds its depth, moves chest and
  shoulders together, and leaves an already-extreme pose inside its ranges.
- `artsheet` verify mode fails on any drift *and* on any breached bound, and
  is part of `ci`'s static-gate group; every §4 check runs over every cell of
  the grid, at every drawn size. Its PNG encoder is proven by round-tripping
  what it writes through `lib/image`'s own decoder rather than by eye.
- The figure digest is asserted by the host suite and by one vertical per
  Tier-1 target (`tests/integration/figure_determinism_*`), so a backend that
  lowered the same arithmetic differently would fail rather than diverge
  quietly.
- Parameter records: bounds enforced, malformed refused, round-trip exact,
  versioned decode total. Fuzz harness over the decoder, since it is
  attacker-reachable in the game (§19.6).
- Designer: a simulated drag produces exactly one durable write and one repaint
  per drained input burst, and touches no state the changed parameter does not
  feed (§28.10, §28.11).
- `miri`: the figure crate forbids `unsafe` outright and the harness carries
  none, so the UB oracle has nothing to interpret in either; re-test if either
  ever gains any. `loom` is not applicable to either half — neither holds
  shared mutable state, an atomic, or an ordering pairing — stated so the
  absence is an answer rather than silence (§19.11).
