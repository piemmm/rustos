# tairix-sapper

Stability tier: **experimental**.

`sapper` — the TAIRiX desktop mine-clearing game. A sapper is the engineer who
clears the mines; the game is the job.

The crate is a host-tested `[lib]` plus the freestanding `Run` binary of the
on-disk `sapper.app` bundle. Everything with behaviour lives in the library —
the board rules, the animation, the layout arithmetic, the painter, and the
best-times store — so the binary only composes it over the window channel, as
`userland/apps/widgets` does.

## Modules

- `board` — the rules. A [`Board`] is a validated `Dimensions` of cells; every
  action returns a `Move`: the cells whose drawn state changed, each tagged with
  the breadth-first **ring** it was reached on, and what the action concluded.
  The ring is what turns a flood fill into a cascade rippling outward from the
  click, and the same list is the repaint's damage set.
- `anim` — what each cell is doing right now, and when the next frame is due.
  A `Move` becomes a **wave**; `Motion::deadline_ns` answers `None` the moment
  the last one ends.
- `layout` — where everything is drawn, in physical pixels, from lengths
  authored at the desktop's reference density and converted through the one
  shared scale. `WindowGeometry` is what the window channel is asked for: the
  client size a board opens at and the range it may be resized within, as one
  answer, so the window a board gets and the geometry the board is drawn in
  cannot disagree.
- `paint` — the drawing, composed from the shared raster primitives against the
  active theme.
- `scores` — the best time on each preset board, in the app-data store.
- `game` — the composition the `Run` binary drives: input in, damage out.

## What it does that the classic game does not

- **The opening move always opens a region.** Mines are laid on the first
  reveal, excluding that cell *and its eight neighbours*, so the first click can
  never lose and can never strand the player on a bare number.
- **Flag chording.** A secondary click on an open number whose covered
  neighbours are exactly its count flags them all — the deduction a player makes
  constantly and then executes one click at a time.
- **Full keyboard play.** Arrows move a cursor drawn as a ring, `Space` reveals
  or chords, `F` marks, `Shift+F` flag-chords, `N` starts a new game, `1`/`2`/`3`
  pick a board.
- **Best times per board**, kept privately per user.
- **It is drawn, not blitted.** Every tile, digit, flag and mine is vector
  geometry rasterised at the active DPI against the active theme, so the game
  re-themes and re-densifies with the rest of the desktop.
- **The cascade, the detonation chain and the victory sweep are animated** as
  waves that ripple outward from the cell the player acted on.

## What it deliberately does not do

- **Guarantee a board is solvable without a guess.** That needs a solver in the
  generator; boards here are uniformly random outside the opening move's safe
  region.
- **Animate hover or press.** A tile lifts and compresses immediately. A game is
  clicked quickly, and a transition on every tile the pointer crosses reads as
  mush rather than as feedback.

## Costing nothing when nothing is happening

The window's park carries the game's own one-shot deadline — the next animation
frame, or the clock's next whole second — and *no deadline at all* when the game
owes neither. A finished board, or one waiting for the first click, takes no
wake and no CPU. Reduced motion is honoured by not starting a wave, so a
suppressed animation is the settled board rather than a second code path.

## Authority

The bundle requests `CAP_CONSOLE_WRITE` and `CAP_SHM` and nothing else: it reads
no filesystem and spawns no process. Best times reach the app-data store over an
ordinary unprivileged call, gated on the bundle's kernel-attested identity. That
write is an IPC round trip, so it is handed to a worker rather than run on the
loop that owes the window a frame; with no worker to be had it happens inline
and is reported, never dropped.

## Testing

`cargo test -p tairix-sapper` covers the rules (including the opening move's
safety over many seeds), the animation's timing and its deadline, the layout at
several scales, the painter over every reachable board state and every point of
every wave, the best-times store against the shared fake app-data service, and
the composed game's input routing, clock, and reported damage.
