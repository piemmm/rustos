## NAME

wintersun — walk a procedurally generated winter world

## SYNOPSIS

`wintersun`

## DESCRIPTION

Opens a desktop window onto a generated world: a top-down view of ground the
machine synthesises rather than ships, lit by a low sun that throws long
shadows down every slope.

Nothing about the world is stored as artwork. Each material the ground is made
of — snow, rock, gravel, heath, tundra — is a handful of numbers the client
turns into a texture as it draws, so the world looks the same on every machine
and takes almost no space on disk. Roads wear into what they cross rather than
sitting on top of it.

Ground that has not been generated yet is drawn as the gap it is and fills in
as it arrives. The client draws what it has rather than stopping to wait, so
the window keeps answering while the world catches up.

Arrow keys or `W`, `A`, `S`, `D` walk. Two held at once walk the diagonal
between them at the same speed, and opposing keys cancel. The view follows you
and stops at the edge of the world rather than sliding off it. Hills too steep
to climb and water too deep to wade turn you aside.

`+` and `-` move the view closer and further, through five steps between one
world cell across eight pixels and one across a hundred and twenty-eight.

`F11` takes the window fullscreen and returns it to whatever it was before, so
a maximised window comes back maximised. `Escape` restores it. `Q` leaves.

The client draws to a frame budget. When it cannot meet one it sheds detail in
a fixed order — particle density, then the light buffer's resolution, then the
materials' detail, then the shadows, then the size it renders at — and the
frame rate is never what gives way. Each step is given back once frames have
been comfortable for a while. The order is fixed so what you see on a slow
machine is predictable rather than a surprise.

A window larger than the software renderer can fill is drawn at up to
2560×1440 and scaled up to the window.

## EXIT STATUS

`0` when you leave. A non-zero status names its reason on standard error: the
world could not be generated, the window could not be opened, or the session's
event channel was lost.

## SEE ALSO

`sapper`
