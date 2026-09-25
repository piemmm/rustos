# tairix-wintersun-app

WinterSun's client shell — the window a player looks through and the frame they
see in it, with the figures standing on the ground (`plans/WINTERSUN.md` WS5,
WS6).

**Stability tier:** `experimental`.

The crate's `[lib]` holds everything with behaviour worth testing: the
projection and its zoom stops (`camera`), the degradation ladder and its
readability floor (`quality`), the render target and its bands (`view`), the
terrain lattice and its splat (`terrain`), the sun and the light buffer
(`light`), the cast and the figure pass (`figures`), the frame that runs every
pass (`frame`), the preset the player walks as (`presets`), the fixed-tick
clock and its interpolation (`pacing`), the input drain (`input`), the window
size states (`shell`), the per-pass budget and the governor that turns the
ladder from it (`budget`), and the cross-target frame digest (`digest`).

The `[[bin]]` is the on-disk `wintersun.app` bundle's `Run` entry point,
composing the library over `lib/window`'s client half. It is a freestanding
pure-Rust program on the Tier-1 bare-metal targets and an inert host stub
elsewhere, so nothing with behaviour lives in it. It reads the player's preset
from the bundle's own `Resources/` once, before the window opens, which is
what its `CAP_FS_ACCESS` is for.

The library does no floating-point arithmetic of its own: it denies
`clippy::float_arithmetic`. The figures it draws are posed in `f64` by
`tairix-wintersun-figure`, which carries its own four-target vertical.
`digest::REFERENCE_DIGEST` is the record of the whole composition — two frames
with figures standing in them — and folds in
`tairix_wintersun_art::digest::REFERENCE_DIGEST` so a change to the ground
moves it too.
