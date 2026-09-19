# The WinterSun ground art

`userland/games/wintersun/art` (`tairix-wintersun-art`) answers what the
ground the world generator decided actually looks like: the palette, the
materials it synthesises rather than ships, the splat that blends them into
pixels, the decals that wear roads and rivers in, and the particles a storm
is made of. It is `plans/WINTERSUN.md` WS4, and the fourth crate of the
`userland/games/` leaf subtree. Stability tier: **experimental**.

## Texture splatting, not tiles

Terrain has no tile grid. Every cell carries a normalised weight vector over
the material set — that is what the world generator's biome stage produces —
and a pixel is the blend of the materials its cell holds.

Blended **by height**, though, not by weight alone. Each material carries its
own surface relief, and the material whose weight-plus-relief is greatest
takes most of the pixel. A linear blend of gravel and grass is a grey that is
neither; a height blend is gravel emerging through grass in patches, which is
what the ground actually does.

It costs nothing extra. A texel is four bytes — red, green, blue, and the
height — so the height rides in a read the splat was making anyway.

A band below the winning score is still blended rather than cut, which is
what anti-aliases the boundary. A hard argmax would give one material per
pixel and stair-step every edge; the whole weight range would be a plain
linear blend with the relief doing nothing. The band is the value between
them.

## Materials are synthesised, not shipped

A material is a palette ramp and four numbers: the world scale of its grain,
the lattice its noise sits on, how far its colour travels along the ramp, how
deep its relief runs and how high it stands. From those, its texture and
height field are generated on the machine that draws them.

- Fifteen materials cost a few hundred bytes in the binary, against megabytes
  of photographic tile sets on disk.
- Being generated they are **resolution-independent**: a mip is the same
  field evaluated at a coarser scale, not a blur of a fixed master.
- The mip a pixel uses is picked from the scale it will be drawn at, because
  a tile whose texels are finer than the pixels sampling them aliases however
  good the filtering is.
- A tile's noise **wraps at the tile's own side**, so a tile drawn end to end
  across a hillside has no seam on the repeat. A sampler that could not wrap
  would produce tiles that cannot tile, so the period is carried by the
  sampler's type rather than by a convention.

### The standing order is the art direction

Because the relief decides which material wins a shared pixel, a material's
standing height *is* a statement about the world: rock stands above gravel,
gravel above sand, sand above water, glacier above snowfield. A river bank
grades from mud through shingle because shingle stands higher than mud, not
because anything special-cases a bank.

## Repetition, and the one field that breaks it

A synthesised tile is finite, so a lookup that was an affine function of
world position would show the tile's period across a large grassland. The
lookup is displaced by a smooth, low-frequency vector field before it is
taken.

One field is all three of the jitters this needs. Its Jacobian carries a
local rotation, a local scale and a local offset together, so there is no
separate rotation to seam at a lattice boundary and no separate scale to
fight it. The field is continuous, therefore the distortion is, therefore
there is no edge anywhere for the eye to find.

It is evaluated at the two ends of a span and interpolated across it rather
than sampled per pixel: it varies over thousands of world sub-units and a
span is a few hundred, so the line is within a sub-unit of the curve and
costs two evaluations instead of one per pixel.

## Roads, rivers and scars are weight, not geometry

The world generator levels the terrain a road runs over and flags the cells
it touches — that is the road's *shape*. What the road is made of and how its
edge meets the ground are drawing questions, and they are answered here: a
spline raises its own material's share of the cells it passes, with a
smoothstep falloff through a feather band outside the carriageway.

Two properties fall out of doing it as weight rather than as geometry:

- **Two roads meeting merge.** Covering takes the maximum, not the sum, so a
  second stamp at the same coverage is exactly a no-op and a junction is a
  road rather than somehow more road than either.
- **Snow needs no second mechanism.** Accumulation raises the snow material's
  weight through the same operation, so it covers ground through the same
  height-weighted blend everything else uses.

A stamp with a step edge reads as a decal, because nothing in a landscape has
one. The distance the falloff is measured on is perturbed by a noise field
keyed on world position, so a road's edge frays at the scale of gravel
scattering into grass. The perturbation is signed, so a decal's reported
extent includes it — a caller buckets decals per tile by that extent, and a
stamp outside the box it reported would simply be missed.

## Particles

Rain, sleet, snow, hail, embers, smoke, dust, sparks, splashes and blown
leaves are one mechanism with ten parameter rows, not ten systems. A row says
how long a particle lives, how heavily it falls, how hard the wind pushes it,
how it is tinted and how it fades.

How many a field may hold is derived from the area on screen and the current
memory-pressure band, never from a number picked in the crate — and particle
density is the first rung of the frame's degradation ladder, so a machine
under memory pressure and a machine dropping frames shed in the same way. An
unreported pressure band reads as critical, so a process that never wired the
protocol carries no weather rather than all of it.

A field at its budget retires its oldest rather than refusing. Refusing would
make a heavy emitter — the one at the centre of what the player is looking at
— starve behind a light one that happened to fill the field first, and the
oldest particle is nearest its own death anyway.

## Everything degrades, nothing fails

Material resolution is total: a tile the cache will not admit degrades to a
coarser mip, and then to the material's flat mid tone at its standing height.
The splat therefore always draws, and a frame is never dropped over a
texture. Tiles live in a `lib/reclaim` cache budgeted from the memory the
machine reported, generationed on the quality they were synthesised at — shed
an octave and every held tile is stale by definition.

Allocation is fallible throughout: a machine too short for a tile gets a
typed error, never an abort.

## No floating point, anywhere

Nothing in this crate is `f32` or `f64`, and `deny(clippy::float_arithmetic)`
makes that a compile error rather than a habit. The whole pipeline — value
noise, smoothstep, the blend, distance-to-segment, particle advection — is
shifts, masks, byte-wide weighted means and one exact integer square root.

That is the right choice twice over. It is the fastest form for the pass with
the largest share of the frame budget. And Rust's integer arithmetic is
exactly specified on every target, so a frame is bit-identical on `x86_64`,
`aarch64`, `riscv64` and `wasm32` **by construction** — the only
target-dependent integer type is `usize`, and nothing here folds one into a
result.

That is why this crate carries no four-target QEMU vertical where
`wintersun/world` and `wintersun/rules` each carry one: those two do
arithmetic whose cross-target identity is a property of the code, and this
one does not. `digest::REFERENCE_DIGEST` still exists, because a digest's
other job is to make an unintended change to the art show up as a moved
number, and the client vertical that hashes a composited frame folds it in —
which is where the cross-target rendering claim belongs, over the whole
picture rather than over one crate.

## What is not here

- **No byte decoding, and therefore no fuzz target.** This crate parses
  nothing: its material rows are compiled in, its weight fields come from the
  world generator's own output, and a decal's path is either that generator's
  road or a player-caused change that `wintersun/net` already bounds-checked
  and fuzzes. The adversarial coverage it does need — that no sequence of
  legal operations leaves a weight field unnormalised or a particle field
  over its budget — is a `proptest` model.
- **No SIMD kernel selection.** The frame budget assumes `lib/cpuops` will
  pick one, but a family with a single portable candidate selects nothing,
  and reaching for per-architecture intrinsics before a measurement says the
  portable kernel misses its budget is speculative optimisation. The
  measurement is the M1 exit criterion; the kernel is already shaped as the
  contiguous span function such a candidate would replace.
- **No camera, no tiling, no lighting.** A caller supplies the two ends of
  each horizontal run and distributes runs over `lib/parallel`; the low sun,
  the shadows and the light buffer are the client shell's.
- **No figures.** The parametric outline primitives are `lib/raster::shape`
  and the rig above them is `wintersun/figure`. Nothing here draws anything
  but ground and particles.
