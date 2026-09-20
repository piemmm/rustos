# Cinder (`cinder.app`)

Cinder is the TAIRiX mascot as a desktop companion: a small rust-and-charcoal
creature who lives in a playpen window and can be let out onto the desktop
itself, where he wanders between windows, chases the pointer, climbs onto
windows, burrows under them, or walks around them.

He is also the first — and so far only — holder of `CAP_DESKTOP_LAYER`, the
authority to be present on the desktop outside a window of one's own. That
authority is documented with the window manager
([window manager](./wm.md)) and the session ([desktop session](./session.md));
this page is the application.

The staged design is `plans/CINDER.md`.

## What he does

**In the pen.** He potters about, grooms, naps, bats a ball, and can be picked
up and put down anywhere on the floor. Clicking him pets him. A labelled button
along the bottom of the pen lets him out and brings him home; the icon-bar menu
offers the same action.

**Out on the desktop.** *Let Cinder out* on his icon-bar menu gives him a
desktop layer surface, and he leaves the pen for the screen. He walks the work
area, notices the pointer when it comes near, and deals with the windows he
meets: climbing onto one and sitting on its title bar, flattening himself and
slipping underneath, or simply going round. Which he does depends on the
window's shape and on his mood.

Clicking him out there pets him too. Only his drawn pixels catch the click: the
transparent margin around him belongs to whatever is behind, so a companion
sitting over a document never swallows a click meant for it.

## Resident, like every icon-bar application

Closing the pen does not quit. Closing it with Cinder inside puts him away;
closing it while he is out leaves him roaming. Clicking his icon opens the pen
again, and *Quit* ends him and takes him off the desktop. This is the
convention `widgets`, `sapper` and `datetime` all follow.

## The elevated camera

Cinder is not drawn in a flat side view. The camera looks **down** on the ground
plane at about 35°, which is what lets him read correctly whether he runs
left-to-right or up-and-down the screen.

One constant carries the whole projection — `GROUND_DEPTH`, the tangent of that
elevation — and everything derives from it:

* he covers screen distance faster side-to-side than up-and-down, so the floor
  reads as a floor rather than as a wall;
* his contact shadow is an ellipse squashed by the same factor, and fades as he
  rises, which is what makes a jump readable;
* his body's parts paint far-first by projected depth, so **the turnaround is
  free**: walking away, his face falls behind his head and vanishes; walking
  toward you it comes forward. There are no front, back and side sprite sets to
  keep in step, and no branch anywhere on which way he is facing.

The sort key is the camera's depth rather than the screen row, so a lifted paw
draws higher without sorting in front of the body it belongs to. Two parts at
the same depth paint in the order the skeleton authored them, which is how a
piece of piping stays behind the flap it edges.

## What he is

A cat, from the mascot sheet: long legs, a narrow wedge of a skull, tall ears
leaning apart, and large tall eyes. His coat is **rust orange all over** — the
dark mass you see is the cowl at his neck and the cape over his shoulders and
flanks, modelled as cloth in its own slate tones and draped over a body that
has its own colour underneath. He is about seventy-six pixels tall at the
reference density, and every offset in the skeleton is in those pixels, so
scaling him is scaling one number.

## How he is drawn

Sixty-four parts, each a body-local position and one shape. The shapes
themselves are not his: they are `lib/raster`'s parametric outline primitives
(`tairix_raster::shape`), shared with the game's figure engine, and they carry
no anatomy — a `Taper` is a taper, and only this crate's rig calls one a leg.
A circle cannot state a cat, so only the genuinely soft-edged parts are discs.
He uses five of the six:

* a **`Superellipse`** — an ellipse pulled some of the way out towards its own
  bounding box, so the corners round while the flats stay flat. The trunk, the
  skull, the haunch, and the eyes;
* a **`Taper`** — tapered from the joint at its own origin down to a rounded
  foot, and rotated about that origin as it swings, so a leg pivots where it
  meets the body instead of sliding beneath it;
* a **`Wedge`** — its tip leaning outwards and its base tucking below the
  skull, so no seam shows where the two meet. His ears;
* a **`ScallopedPanel`** — a panel of cloth with a scalloped hem, which is what
  stops the cape reading as a card taped on;
* a **`Splat`** — a radial blob with a small deterministic angular ripple, for
  the cheek ruffs and the tail's banded plume, where a crisp edge would read as
  moulded plastic. It is the one primitive with no traced outline; the ripple
  comes from the part's own identity, so it is the same every frame, and fur
  that re-rippled would boil.

The sixth, `BevelledPanel`, is a hard-edged panel with a bevelled rim — armour
drawn as a superellipse reads as flesh — and he has no use for it.

Every outline is symmetric about its own vertical axis, which is what keeps the
turnaround free — an asymmetric shape would need mirroring, and mirroring is
the per-direction branch the camera exists to avoid. The wedge is the
deliberate exception: it leans, and the pair leans apart rather than either one
being handed.

Outlines are filled through `lib/raster`'s one anti-aliased scan converter, so
there is no second rasteriser and the edge quality is the desktop's own. Every
shape's vertex count is bounded and asserted at build time, so the outline path
touches no allocator at all.

Each limb's joint is also where the trunk carries a shoulder or haunch mass, so
a swinging shank can never open a gap at the shoulder — the defect that made an
earlier model's legs look detached.

The face carries the detail, because that is what is recognisable at this size:
cream cheek ruffs for the eyes to read against, a lash line around each,
tall amber irises with tall pupils and two catchlights from one light, cream
brow markings, a small cat muzzle, and a mouth whose corners lift with his
mood. A lid *flattens* an eye rather than shrinking it, and a shut eye keeps a
sliver, so a blink reads as a closed lid instead of a hole in the mask.

The palette is the mascot sheet's: `#F0782A`/`#D24B17`/`#BB3A10` fur,
`#F2E1C8`/`#BDA18A` cream, `#2A2B2F`/`#1A1C1F`/`#0E1013` cloth, a dark rust
paw, and amber eyes.

## Getting about

Out on the desktop he holds a home — the spot he was let out at, which is the
only screen position the desktop actually gave him, since an application is
never told where its own window sits. Every journey but a chase runs through
one destination slot, so arriving retires it and a walk home simply stops
rather than each intent needing its own stopping rule.

Two properties keep him honest:

* **a pace is only a pace towards somewhere.** With nothing to walk to he
  stands, rather than drifting along whatever heading he last held;
* **his legs are driven by the ground he covered, never by the pace he
  intended.** Held at the work area's edge, blocked by a window, standing at
  his destination, or airborne mid-leap, he stops stepping instead of treading
  air — and any future way of being stopped is covered by the same one rule.

An edge deflects him rather than pinning him: the destination he was walking to
lies outside the work area, so clamping alone left him pressed against the
boundary for good.

All of this is in the crate's host-tested library rather than in its `Run`
binary, because it is behaviour and behaviour has to be testable without a
screen.

## Mood

Three needs — rest, play, company — drift with what he is doing and decide what
he wants next, drawn from a seeded generator so his behaviour is reproducible
and testable rather than merely watchable.

There is deliberately no feeding economy, no breeding, and no inventory. Nothing
goes wrong if you ignore him. The needs exist so you can tell at a glance what
sort of mood he is in; a companion that becomes a chore has stopped being one.

His mood, and whether he was out, are kept in his app-data store and restored
next time.

## Being refused

If the account's grants do not carry `CAP_DESKTOP_LAYER`, if there is no
graphical session, or if the seat already holds its companions, *Let Cinder out*
is refused. The reason is stated on `stderr` and in the pen, the pen keeps
working, and Cinder stays in. A denied optional action is an answer, not a
fatal error.
