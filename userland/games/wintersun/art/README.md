# tairix-wintersun-art

WinterSun's ground art: the palette, the materials it synthesises rather than
ships, the height-offset splat that blends them into pixels, the decals that
wear roads and rivers into the weight field, and the particle vocabulary
(`plans/WINTERSUN.md` WS4). Stability tier: **experimental** — nothing has
shipped, so a parameter row or a kernel constant may still change in place.

`no_std` (needs `alloc`), `forbid(unsafe_code)`, and **no floating point at
all** — `deny(clippy::float_arithmetic)` makes that a compile error.

## The four ideas

| | |
|---|---|
| **Materials are synthesised, not shipped** | A material is a palette ramp and four numbers, from which its texture and height field are generated on the machine drawing them. Fifteen of those are a few hundred bytes in the binary rather than megabytes of photographic tiles on disk, and being generated they are resolution-independent: a mip is the same field sampled coarser, not a blur of a fixed master. |
| **A pixel is decided by height, not weight alone** | Every material carries its own surface relief, and where one stands proud of its neighbours it takes the pixel outright. That is what makes gravel emerge through grass in patches rather than the two averaging into a grey that is neither — and it is free, because the height rides in the fourth byte of a texel the splat was reading anyway. |
| **Roads and rivers are weight, not geometry** | A spline raises its own material's share of the cells it passes, through the one mutation everything that changes the ground goes through. So a road *wears into* grass with a frayed edge, two roads meeting merge rather than overlap, and snow settling later needs no second mechanism at all. |
| **One warp field breaks the repetition** | A synthesised tile is finite, so an affine lookup would show its period across a large grassland. A smooth low-frequency vector field displaces the lookup; its Jacobian carries a local rotation, scale and offset together, so there is no separate jitter to seam at a lattice boundary and no discontinuity for the eye to find. |

## Modules

| Module | What it owns |
|---|---|
| `palette` | The WinterSun colour vocabulary, as three-tone ramps rather than flat colours — because a surface under a low sun is never one colour, and a palette of flat colours makes every consumer invent its own darkening. |
| `noise` | The one integer lattice under every grain, warp and frayed edge: keyed value noise, smoothstep-interpolated, fBm, and wrapping at a stated period so a tile can tile. |
| `material` | The fifteen parameter rows, the mip chain, the `Quality` octave knob, and the synthesis that turns a row into a seamless RGB+height tile. |
| `cache` | The `lib/reclaim`-governed tile cache, keyed on `(material, mip)` and generationed on `Quality`. |
| `weight` | The splat field's weight vector and `cover`, the single mutation decals, snow accumulation and scorch marks all go through. |
| `decal` | Polyline stamps into that field: exact integer distance-to-segment, a smoothstep falloff, and a noise-frayed edge. |
| `splat` | The span kernel — plan a run once, step it per pixel — the height-offset blend, and the anti-repetition warp. |
| `particle` | Ten kinds, their parameter rows, and a bounded field whose budget is derived from the area on screen and the memory-pressure band. |
| `digest` | The scripted scene and its reference constant. |

## Why there is no cross-architecture vertical here

The world generator and the simulation each carry a four-target QEMU
vertical, because each does arithmetic whose identity across targets is a
property of the *code*: one works in `f64`, the other in integers with a
single `f64` conversion, and "should be identical" and "is identical" are
different claims.

Nothing here is floating point, the lint enforces it, and the only
target-dependent integer type is `usize`, which nothing folds into a result.
Bit-identity therefore *follows from the language*, and four emulated
machines would be confirming Rust rather than this crate.
`digest::REFERENCE_DIGEST` still exists, because a digest's other job is to
make an unintended change to the art show up as a moved number. The
cross-target *rendering* claim is made where it belongs — over a whole
composited frame, in the client vertical (`plans/WINTERSUN.md` WS5) — and
that vertical folds this constant in.

## Memory

Resident cost is what is on screen, never the world's extent. Tiles live in a
`lib/reclaim` cache budgeted from the memory the machine reported and given
back through the system's pressure bands. Resolution is **total**: a tile the
cache will not admit degrades to a coarser mip and then to the material's flat
tone, so the pass always draws and a frame is never dropped over a texture. A
particle field's budget is `area / band`, capped by a containment bound so a
camera pulled back over a realm cannot turn weather into an allocation path.

Allocation is fallible throughout: a machine too short for a tile gets a typed
`ArtError::OutOfMemory`, never an abort.

## What is not here

- **No byte decoding, and therefore no fuzz target.** This crate parses
  nothing. Its material rows are compiled-in, its weight fields come from the
  world generator's own output, and a decal's path is either the generator's
  road or a player-caused change that `wintersun/net` already bounds-checked
  and fuzzes. The adversarial coverage it does need — that no sequence of
  legal operations leaves a weight field unnormalised or a particle field over
  budget — is `tests/proptest_model.rs`.
- **No SIMD kernel selection.** The plan's frame budget assumes `lib/cpuops`
  will pick one, but a family with a single portable candidate selects
  nothing, and reaching for per-architecture intrinsics before a measurement
  says the portable kernel misses its budget is the speculative optimisation
  the charter forbids. The measurement is the M1 exit criterion (WS5); the
  kernel is already shaped as the contiguous span function such a candidate
  would replace.
- **No camera, no tiling, no lighting.** A caller supplies the two ends of
  each horizontal run and distributes runs over `lib/parallel`; the low sun,
  the shadows and the light buffer are the client shell's (WS5).
- **No figures.** Parametric outline primitives are `lib/raster::shape`
  (`plans/FIGURE.md` FG1) and the rig above them is `wintersun/figure` (WS6).
  Nothing here draws anything but ground and particles.
