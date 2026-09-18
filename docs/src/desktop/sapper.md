# Sapper (`sapper.app`)

Sapper is the desktop's mine-clearing game: a grid of covered cells, some
hiding a mine, cleared by deduction from the counts the safe cells show. A
sapper is the engineer who clears the mines, which is the job the game is.

It is a windowed application like any other — its own bundle, its own `Run`
binary, and only the authority its signed manifest asks for
(`CAP_CONSOLE_WRITE` and `CAP_SHM`). It reads no filesystem and spawns no
process.

## The rules, and where they differ from the classic game

The board is `cols × rows` cells holding `mines` mines. Revealing a cell that
is not a mine opens it, showing how many of its eight neighbours are mines;
revealing one that is ends the game. Opening every cell that is not a mine wins
it.

Two things are deliberately different from the game this clones.

**The opening move always opens a region.** Mines are laid on the *first
reveal*, excluding the clicked cell **and its eight neighbours**. The first
click can therefore never lose, and can never strand the player on a bare `1`
with nothing to deduce from. The classic game excludes at most the clicked cell.

**Flag chording.** A secondary click on an open number whose covered neighbours
are exactly as many as its count flags them all at once. It is the deduction a
player makes constantly and then executes one click at a time; the classic game
offers only the reveal half of the pair.

Boards are otherwise uniformly random. Guaranteeing a board is solvable without
a guess would need a solver in the generator and is a deliberate non-goal.

## Boards

| Board | Size | Mines | Shortcut |
|---|---|---|---|
| Beginner | 9 × 9 | 10 | `1` |
| Intermediate | 16 × 16 | 40 | `2` |
| Expert | 30 × 16 | 99 | `3` |

A size of the player's own is a validated `Dimensions`: each side within
`MIN_SIDE..=MAX_SIDE`, at least one mine, and no more mines than fit outside
the opening move's safe region — so a guaranteed-safe first click always
exists. Those bounds are validation of a value that may reach the game from a
hand-edited settings document, not capacities to raise.

## Input

| Action | Pointer | Keyboard |
|---|---|---|
| Move the cursor | — | arrow keys |
| Reveal | primary click | `Space` / `Enter` |
| Mark (flag → question → clear) | secondary click | `F` |
| Chord (reveal around a satisfied number) | primary or middle click on an open number | `Space` on an open number |
| Flag-chord (flag around a determined number) | secondary click on an open number | `Shift+F` |
| New game | the button between the readouts | `N` |
| Choose a board | the icon-bar menu | `1`, `2`, `3` |

A press only *shows* the intent; the release commits it, so a pointer dragged
off the cell takes the move back, exactly as a button does. A flagged cell is
protected from a reveal, and a cascade stops at one rather than sweeping the
player's own claim away.

## The window

One header band over the grid. The counter on the left reads mines less flags
placed — signed, so over-flagging says so rather than clamping at zero and
lying. The clock on the right starts on the first *reveal* (the board does not
exist until then, so there is nothing yet to time) and freezes when the game
ends. The button between them starts a new game and its face reads the phase:
neutral while playing, anxious while a cell is held, a visor on a win, crossed
eyes on a loss.

Every length is authored in logical pixels at the reference density and
converted through the one shared `tairix_geometry::Scale`, so a dense display
gets a bigger board rather than a smaller one. The window is resizable: the
cell side is derived from the space available, between a floor where the cell
stops being legible and a ceiling where it stops growing.

The size the window opens at and both ends of that range are one answer
(`WindowGeometry::resolve`), because they are one decision read at three cell
sides — and stating them separately is how a window comes to be opened
outside its own declared range. The opening size is additionally capped to
the display, since a board taller than the screen puts its own last rows out
of reach. The window manager enforces the range, so a drag stops at the floor
and a drag or a maximize stops at the ceiling rather than growing a window
that is all margin.

A resize the *user* is dragging is adopted as given and never answered with a
size of the application's own, which would fight the drag — but adopting it
means re-mapping the frame region onto it, so the board is always laid out in
the extent it is actually drawn into. The board it asks for changes with the
board being played and with the desktop's density, and each of those restates
the range to the window manager as well as re-shaping the window; a window
opened after one of them was chosen with no window on screen therefore opens
at the size that board needs, laid out for it before its first frame.

The two readouts are drawn as seven-segment instruments — a dark plate on
either appearance, because unlit segments need a dark plate to read as unlit.
Everything else resolves from the active theme: a covered tile stands one clear
step off the surface, lit from above on both appearances, and the eight
adjacency colours are the game's own palette, tuned per appearance exactly as
the terminal owns its ANSI scheme.

## Animation, and what it costs when nothing is happening

Every board action returns the cells it changed, each tagged with the
breadth-first **ring** it was reached on. That ring is what the animation is
timed from, so a flood fill ripples outward from the click instead of blinking
into existence, and a detonation chains outward from the struck mine. The same
list is the repaint's damage set — one answer, not two — so a cascade across
half an Expert board repaints those cells and nothing else.

| Wave | Started by |
|---|---|
| Reveal | cells opened — the cover lifts and fades off the face beneath |
| Mark | a flag or question landing, with an overshoot so it plants |
| Detonate | a struck mine — the mines pop outward behind a shockwave |
| Victory | the last safe cell — a sweep of light across the finished board |
| Rejected | an action the rules refused, shaken on the cell it was refused on |

Every wave is finite and begins at a state change; none is a loop. The window's
park carries the game's own one-shot deadline — the next animation frame, or
the clock's next whole second — and **no deadline at all** when the game owes
neither, so a finished board or one waiting for its first click takes no wake
and no CPU.

Reduced motion is honoured by not starting a wave at all. The board state is
applied before the wave is offered, so a suppressed wave means the settled
board immediately; there is no second code path, and a host test asserts the
two produce identical pixels.

Hover and press are *not* animated. A game is clicked quickly, and a transition
on every tile the pointer crosses reads as mush rather than as feedback.

## Best times

The best time on each of the three standard boards is kept in the application's
own app-data store, under `best.beginner`, `best.intermediate` and
`best.expert`. The store is private to this bundle and gated on its
kernel-attested identity, so no other application the user launches can read or
write it. A custom board keeps no time: no two custom boards are the same game.

A stored value the registry refuses — a time outside its bounds, or text that is
not a number — leaves that entry empty and is named on the standard error
stream, so one corrupt entry costs only itself and can never become a time
nobody played.

Writing a best time is an IPC round trip to the settings service, so it is
handed to a worker rather than run on the loop that owes the window a frame.
The desk is latest-wins with at most one write in flight. Where the kernel
grants no worker thread or no wake pipe, the write happens on the loop instead
— slower under load, never wrong, and never a dropped record.

## Where the code is

`userland/apps/sapper`. The host-tested `[lib]` holds everything with
behaviour: `board` (the rules and the ring-tagged `Move`), `anim` (the waves
and the deadline), `layout` (the geometry), `paint` (the drawing), `scores`
(the best times) and `game` (the composition input is routed into). The `Run`
binary composes that model over the window channel and owns no game logic of
its own.
