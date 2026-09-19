# tairix-wintersun-app

WinterSun's client shell — the window a player looks through and the frame they
see in it (`plans/WINTERSUN.md` WS5).

**Stability tier:** `experimental`.

The crate's `[lib]` holds everything with behaviour worth testing: the
projection and its zoom stops (`camera`), the degradation ladder (`quality`),
the render target and its bands (`view`), the terrain lattice and its splat
(`terrain`), the sun and the light buffer (`light`), the tiled frame
(`frame`), the fixed-tick clock and its interpolation (`pacing`), the input
drain (`input`), the three window size states (`shell`), the per-pass budget
and the governor that turns the ladder from it (`budget`), and the
cross-target frame digest (`digest`).

The `[[bin]]` is the on-disk `wintersun.app` bundle's `Run` entry point,
composing the library over `lib/window`'s client half. It is a freestanding
pure-Rust program on the Tier-1 bare-metal targets and an inert host stub
elsewhere, so nothing with behaviour lives in it.

No floating point: the library denies `clippy::float_arithmetic`, so a frame
is bit-identical on every Tier-1 target by the language's own rules.
`digest::REFERENCE_DIGEST` is the record of that, and folds in
`tairix_wintersun_art::digest::REFERENCE_DIGEST` so a change to the ground
moves it too.
