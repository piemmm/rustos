# CINDER — the desktop companion, and the authority that makes one possible

Binding under `AGENTS.md`. Two things: the **desktop-layer authority** every
companion-shaped application needs, and **Cinder**, the first holder of it.

Read `plans/NEW-TASKBAR.md` (the resident icon-bar convention), `plans/APPWIN.md`
(the window channel), and `plans/GUI-CONTROLS-DESIGN.md` (control behaviour)
before touching either.

## Ledger

| Item | What it is | Status |
|---|---|---|
| CD1 | `CAP_DESKTOP_LAYER` (id 47) and its place in the session baseline | done |
| CD2 | The three layer operations on `WINDOW_ENDPOINT`, and the two feed events | done |
| CD3 | Containment bounds: surface side, per-client and per-seat counts, terrain plates | done |
| CD4 | Compositor mechanisms: alpha-shaped pointer catch, focus refusal, `stack_below`, terrain enumeration | done |
| CD5 | `lib/window`: the capability seam, the engine's gate, the host bridge, the client half | done |
| CD6 | Session policy: clamp, live-scale re-check, stacking, the two coalesced feeds, trusted-surface suppression, the audit records | done |
| CD7 | The elevated camera and the five shapes a part is drawn as | done |
| CD8 | The body, the gait, the mind, and the roaming rules | done |
| CD9 | The desktop route planner: over, under, around, and the depth flip | done |
| CD10 | The playpen, the painters, the saved mood | done |
| CD11 | The `cinder.app` bundle: manifest, icon, help in every required locale, the `Run` binary | done |
| CD12 | QEMU vertical: composite at both depths, silhouette-vs-margin click, feeds stopping under the lock screen | planned |
| CD13 | A second holder of the authority — a notification perch, a dock widget | planned |

## Part A — the desktop-layer authority

### A1. What the capability is, and what it is not

`CAP_DESKTOP_LAYER` grants **presence on the desktop outside a window of one's
own**: a surface placed in *screen* coordinates, stacked relative to other
principals' windows, with the desktop-geometry and pointer feeds that placement
requires.

Being *undecorated* is not the privileged part and never was.
`WindowRequest::CreatePopup` already opens a furniture-less surface with no
capability at all, and is safe because it is anchored to a window the caller
owns, offset from that window's client origin (an app is never told its own
screen position), and clamped onto the screen by the session. The capability
grants exactly what the popup withholds.

It passes the charter's three tests for a new capability: it guards a class of
operations rather than one object; it lands with a live holder (`cinder.app`)
and a live enforcement point in the same change; and no existing capability
expresses it — `CAP_DISPLAY` is the raw framebuffer the session itself holds,
and `CAP_SEAT_ADMIN` administers seats rather than drawing on one.

It is in `tairix_users::SESSION_BASELINE`, so the ceiling ∩ *signed manifest*
intersection is what does the work: a bundle can only obtain it by asking for it
in a manifest the system signed.

### A2. Why it is safe to hand out — the five structural controls

It is a user-interface spoofing primitive. It is bounded by construction, not by
trusting its holder, and each control closes a distinct attack:

| Attack | Control |
|---|---|
| Reproduce a trusted prompt | `DESKTOP_LAYER_MAX_SIDE_LOGICAL` (256) is below the narrowest surface the session draws for a trusted decision (the elevation and confirmation prompts, both 460 logical px wide). `layer::LAYER_FITS_UNDER_TRUSTED_SURFACES` asserts that at compile time against each, so shrinking a prompt below the bound fails the build. |
| Capture a keystroke | A layer surface is never in the focus rotation. `Compositor::set_focusable(false)` makes a press neither focus nor raise it, and `InputRouter::focus` refuses it, so there is no second route in. A pixel-perfect lookalike is typed past. |
| Clickjack | `PointerCatch::Shape` hit-tests against the window's **own content alpha**, so only drawn pixels catch the pointer. A released window catches nothing — it draws nothing, so it fails closed. |
| Cover what the user acts through | The highest depth is `stack_below(companion, taskbar)`, never the front. A menu or tooltip raises over it. |
| Watch a credential being typed | Both feeds stop and the surface is hidden whenever the session shows a trusted surface (lock, picker, elevation prompt). A suppressed pointer sample is *dropped*, never queued for replay. |

### A3. Where the capability is enforced, and why there

**Not** by a restricted-sender endpoint. The kernel makes a restricted-sender
bind unconditionally require `CAP_IPC_BIND_PRIVILEGED`, and the seat lease
substitutes only for the *reserved-id* half of that gate
(`CallEndpoint::create_seat_attested`). A session is an ordinary user process
whose ceiling never carries the privileged bind, so it physically cannot bind
one.

The gate is therefore in `WindowServer::serve`, checked against the caller's
kernel-attested `Origin::capabilities()` summary before dispatch touches any
state — the same unforgeable fact the kernel would have used, and the same shape
`seatmgr` uses for `CAP_SEAT_ADMIN` and `lib/display` for `CAP_SYSINFO_HW`. It is
re-checked on **every** layer operation rather than only at open, so a revoked
grant stops the surface at its next request.

### A4. Why the operations ride `WINDOW_ENDPOINT`

A layer surface *is* a window in the session's one registry: it has a shm
region, frames, damage, presents, an event route, an owner, a budget, and a
teardown. `Present` and `Close` act on it unchanged, and retiring one is an
ordinary close.

A separate endpoint would have meant a second bind, exit code, wait-set token,
capacity, decode path, and — worse — a second registry that had to agree with
the first about id allocation, ownership, budget, and teardown-on-exit. Two
registries that must agree is how an orphaned surface gets in. Three operations
were added instead:

* `OpenLayer` — the surface, a screen point, and a depth. Answers the ordinary
  create reply.
* `PlaceLayer` — move and re-stack together, because a companion changes both
  in one step and splitting them would let a frame land at the old depth.
* `TakeTerrain` — pull the visible windows' rectangles, back-to-front.

And two events: `TerrainChanged { generation }` and `LayerPointer { x, y }`.

### A5. The feeds

**Terrain** is *pulled*, not pushed: the event carries a generation, so a burst
of window movement costs one small event and the holder asks for the answer when
it is ready. A holder that never asks is told nothing and costs the session
nothing. The reply carries rectangles and stacking order only — no titles, no
ids, no owners, no pixels — all of it already visible on screen.

Change detection compares the desktop's actual shape against a bounded snapshot
once a frame, rather than hooking every move, resize, open, close, raise and
hide that could alter it. An exact comparison cannot miss a change; a hook on
each mutation site eventually would.

**Pointer** is sampled once a frame from the tracked pointer and delivered only
when it moved. Position only: no buttons, no modifiers, no timestamp, nothing
about the window underneath. A press *on* the surface arrives as an ordinary
`Pointer` event with a surface-local position, so the feed never carries a click.

### A6. What is audited

Every open, refusal and retirement is a security decision on the session's log
(`LAYER_OPENED`, `LAYER_REFUSED`, `LAYER_RETIRED`), naming plainly that the
holder receives pointer position. The feeds starting and stopping as a trusted
surface comes and goes is its own record (`LAYER_FEEDS`). The gate's own
refusal reaches the log through `WindowHost::layer_refused`, because the gate is
enforced session-side and would otherwise be the one refusal nothing recorded.
The decision queue is bounded; beyond it the *count* still rises, so a flood is
visible as a flood rather than silently dropped.

## Part B — Cinder

### B1. The camera

An elevated, isometric-style camera looking **down** on the ground plane at 35°,
so the creature reads correctly whether he runs left–right or up–down. One
constant carries it: `GROUND_DEPTH = tan 35° ≈ 0.70`.

* Ground positions are screen pixels. A body-frame velocity's depth component is
  scaled by `GROUND_DEPTH`, so he covers screen distance faster side-to-side
  than up-and-down — which is what makes a foreshortened floor read as a floor.
* A part at body-local `(fwd, side, up)` with heading `h` projects to
  `sx = x + fwd·cos h − side·sin h`,
  `sy = y − (fwd·sin h + side·cos h)·GROUND_DEPTH − up`.
* **The turnaround is free.** Parts paint far-first by projected depth, so
  walking away his face parts fall behind his head and vanish; walking toward
  the camera they come forward. No front/back/side sprite sets, no `cfg`, no
  second code path. The sort key is the camera's depth and *not* the screen row:
  a raised part draws higher without becoming further away. A tie breaks on the
  skeleton's own order, so a piece of piping stays behind the flap it edges —
  the cape's flap and its trim sit at equal depth at exactly the commonest
  heading, facing the camera.
* Every outline is **symmetric about its own vertical axis**, which is what
  keeps the above true: an asymmetric shape would need mirroring, and mirroring
  is the per-direction branch the camera exists to avoid. The ear is the
  deliberate exception — it leans, and the pair leans apart rather than either
  one being handed.
* A contact shadow sits at his ground point, squashed by `GROUND_DEPTH` and
  fading as he rises, which is what makes a jump readable.

### B2. The creature

A **cat**, not a red panda: long legs, a narrow wedge of a skull, tall ears
leaning apart and set close, and large tall eyes. His coat is **rust orange all
over**. The dark mass on screen is the *garment* — the cowl at his neck and the
cape over his shoulders and flanks — modelled as cloth in its own slate tones
and drawn over a body that has its own colour underneath. A trunk painted in
the cloth's tones is a black cat in a black coat, which is what an earlier
attempt drew.

Sixty-four parts, each a body-local position and one of five shapes
(`src/shape.rs`). A circle cannot state a cat, so only the genuinely
soft-edged parts are discs:

| Shape | What it states | Where |
|---|---|---|
| `Mass { rx, ry, square }` | an ellipse pulled `square` of the way out to its own bounding box, so corners round while flats stay flat | trunk, skull, haunch, eyes, nose |
| `Limb { length, top, foot }` | a taper from the joint at its own origin to a rounded foot, rotated about that origin as it swings | the four legs |
| `Ear { half_width, height, lean }` | a wedge with a leaning tip and a base tucked below the skull | the ears |
| `Drape { rx, ry, folds }` | a panel of cloth with a scalloped hem | the cape and the cowl's fold |
| `Fur { radius }` | a radial splat with a deterministic angular ripple | cheek ruffs, the tail's banded plume, the crown |

Outlines are filled through `lib/raster`'s one anti-aliased scan converter —
no second rasteriser here. Every shape's worst-case vertex count is asserted
against the outline buffer at **build** time, so a generator given more detail
fails the build rather than silently truncating a ring into a shape nobody
authored; the buffers are `lib/inline` fixed arrays, so the outline path
touches no allocator.

**Each limb's joint is also where the trunk carries a shoulder or haunch
mass**, from the one `LIMB_JOINTS` table, so a swinging shank can never open a
gap at the shoulder. Legs that floated below the body were the defect this
closes, and a stack of discs is all a disc model could offer.

The face carries the detail, because that is what is recognisable at a
companion's size:

* **cream cheek ruffs** for the eyes to read against, drawn as fur so their
  edge is spiky rather than moulded;
* a **lash line** around each eye. Without it the amber sits on cream and the
  eyes vanish at this size;
* **tall** amber irises with tall pupils and two catchlights offset the same
  way in both eyes, so the pair reads as wet under one light. Taller than wide
  is the shape the sheet's five expressions are all drawn with;
* **cream brow markings**, a small cat muzzle, and a mouth of five ink points
  whose corners lift with the pose's `smile`, so it curves rather than sliding
  upward;
* a **banded tail**, alternating light and dark, curling up behind and forward
  over his back.

A lid **flattens** an eye rather than shrinking it, and a shut eye keeps a
sliver: a dropped part leaves a hole in the mask, a flattened one reads as a
closed lid. The flag that marks an eye lives on the part, not on an index range
over the table, so reordering the skeleton cannot silently stop meaning the
eyes.

Pose parameters — gait phase, crouch, lift, head yaw and pitch, ear flop, tail
sway, smile, eye state — are data, not code paths. The smile is driven from the
mind's `cheer` (play and affection, not energy: tiredness is what the eyes say,
not unhappiness).

### B3. The mind

Three needs (energy, play, affection) and a closed set of intents (wander, sit,
groom, nap, chase, pounce, climb, burrow, come home), driven by an **injected**
`RandU64`, so every decision is deterministic and host-testable.

Deliberately not a pet-sim: no feeding economy, no breeding, no inventory, and
no stat the user must manage. The needs exist to make his behaviour legible, not
to be optimised. A companion that becomes a chore has stopped being one.

### B3a. Getting about (`src/roam.rs`)

The per-frame join of mind, world and gait. It is in the **library**, not the
`Run` binary: a frame advance that lived in the freestanding binary was
reachable by no host test at all, which is how a companion that walked on the
spot survived a green pipeline three times over. It holds no window, no
surface and no channel — the caller draws whatever it answers.

* **Home is the spot he was let out at.** An application is never told where
  its own window sits, so that release point is the only screen position the
  desktop actually gave him. Home as "his own current position" made
  `heading_towards(at, at)` answer zero and drove him off screen-right for
  good.
* **Every journey but a chase runs through one destination slot**, retired the
  moment he stands within `ARRIVED` of it and *before* the next is resolved. So
  a wander draws its following leg and a walk home simply stops, rather than
  each intent carrying its own stopping rule. A chase reads the live pointer
  instead, because a pointer moves.
* **A pace is only a pace towards somewhere.** With nothing to walk to he
  stands, rather than drifting along whatever heading he last held.
* **The legs follow the ground he covered, never the pace he intended.** Held
  at an edge, blocked, standing at his destination, or airborne mid-leap, he
  stops stepping instead of treading air — and any future way of being stopped
  is covered by the same one rule.
* **An edge deflects him** (`Area::deflect`) rather than pinning him: the
  destination he was walking to lies outside the work area, so clamping alone
  left him pressed to the boundary for good. A corner reflects both edges,
  which is a turn back the way he came.
* **An approach walks to the plate's own nearest point** — the very point the
  reach test measures against (`Plate::nearest`, shared by both) — so a
  crossing cannot inherit a stale destination and stall short of the window it
  is crossing.

### B4. The route

`Floor → Approach → {Climb → Perch → Descend | Burrow | Skirt} → Floor`.

The planner picks over/under/around from the plate's geometry and his mood. The
**depth flips at the leap's apex**, not at its start or its finish, so he
appears over the window edge as he comes down onto it rather than popping in
front of it as he takes off. The state machine owns the depth for exactly that
reason: deriving it from the state means no caller can set it at the wrong
instant.

### B4a. The pen's control strip

The pen carries a labelled `lib/controls` **button** along the bottom — *Let
Cinder out* / *Bring Cinder home* — because the icon-bar menu is not where
anyone looks for an application's main action. The strip is chrome drawn over
the room, and a press in it never reaches the floor behind it. Both the button
and the icon-bar row resolve to one place, so they cannot disagree about what
the action does or leave the bar's label behind.

### B5. Resident, and degrading gracefully

Closing the pen never quits. Cinder is a resident icon-bar application, so
closing the pen with him inside puts him away and closing it while he is out
leaves him roaming; *Quit* ends him and takes him off the desktop.

If the account's ceiling strips `CAP_DESKTOP_LAYER`, or there is no graphical
session, or the seat's companion total is reached, "let Cinder out" is
**refused**: the reason is stated on `stderr` and in the pen, the pen keeps
working, and the app does not die over a denied optional action.

## CD12 — the QEMU vertical, and what it must assert

Not landed. Every piece of *logic* is covered by host tests — the projection and
its turnaround, the shapes and their build-time vertex bounds, the gait, the
roaming rules of §B3a, the mind under a seeded generator, the route planner's
over/under/around choice and its depth-flip instant, the pen interaction and
its strip geometry, the pen button's label reaching pixels, the layer codec's
every fail-closed decode, the containment bounds, the alpha-shaped hit test,
`stack_below`, terrain generation, and the pointer coalescing — plus a fuzz
harness over the whole wire surface. What no host test can show is that those
pieces are *wired to each other on a real machine*.

**Keep behaviour out of the `Run` binary.** Three rounds of defects — a
labelless button, a companion walking on the spot, a walk home with no
destination — were all visible to anyone watching the desktop and invisible to
the whole pipeline, because the code that held them sat in
`#[cfg(freestanding)] mod program`, which no host test compiles. The movement
moved to `src/roam.rs` for exactly that reason. A `Run` binary should compose
library parts and speak to the window channel; anything it *decides* belongs in
the library, and the vertical below is not a substitute for that.

The vertical must boot the production aarch64 desktop against a planted root
carrying the signed `cinder.app` bundle, and drive it blind through the QEMU
monitor:

1. launch `cinder`, confirm the pen window is composited;
2. confirm the pen button's **label** is on screen — ink inside the button's
   own rect, read back from the composited frame, not merely that a button was
   rendered. Host tests now pin this three ways (the plate's own guard, the
   strip's height against `Button::height`, and ink counted inside the rect),
   and the vertical is what proves the glyphs survive the real font service;
3. *Let Cinder out*, and confirm a layer surface is composited at `Below` —
   its pixels present where an application window is not, and absent where one
   is;
4. drive him onto a window and confirm the same surface composites at `Above`;
5. click his silhouette and confirm the press reaches him (a pet), then click
   his transparent margin at the same surface and confirm the press reaches the
   window beneath instead;
6. raise the lock screen and confirm the surface stops compositing and both
   feeds stop, then dismiss it and confirm they resume.

Steps 5 and 6 are the ones worth the machine: they are the two controls that
make the authority safe, and each depends on the compositor, the session and the
application agreeing.

## Deliberate non-goals

* No second rounded-corner, rasterisation, or decode path: the companion is
  composited by the one compositor through the one raster library.
* No numeric layer index and no "topmost": both would be ways to climb above
  surfaces the session owns.
* No keyboard. A layer surface that could be typed into is the whole attack.
