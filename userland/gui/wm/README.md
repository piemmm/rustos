# rustos-wm

The RustOS compositing window manager (`userland/gui/wm`, `AGENTS.md`
§10). It composes per-window surfaces into a single scan-out frame and
presents it through a capability-gated
`rustos_abi::driver::display::Display` driver. All compositing happens in
user space; no non-GUI crate depends on this crate (`AGENTS.md` §17.3).

## Status

First Stage 7 increments — the **compositor core** and the **input
router**:

- Premultiplied-alpha pixels (`color`) with the Porter–Duff *over*
  operator: correct per-surface and per-region transparency.
- Surfaces (`surface`): dense premultiplied pixel buffers.
- Anti-aliased rounded corners (`corner`) via deterministic
  supersampling, with a square-corner opt-out — the single
  rounded-corner path the taskbar reuses (`AGENTS.md` §2.2).
- Damage tracking (`damage`): only changed pixels are recomposited.
- The `Compositor`: a z-ordered window stack composited over an opaque
  background into a `DisplayMode`-shaped byte frame, presented through a
  `Display` seam.
- Input routing (`input`): the `InputRouter` tracks the pointer and the
  focused window, raises and focuses the window under a primary press
  (click-to-activate), and drives explicit interactive window
  move-grabs; `Compositor::window_at` is the top-most hit-test.

GPU acceleration, theming, and the taskbar build on this core in later
Stage 7 increments.

## Properties

- `no_std` (+ `alloc`); the only dependency is `rustos-abi` (`AGENTS.md`
  §17.4).
- No `unsafe`; no `unwrap`/`expect`/`panic!` in production paths — every
  fallible entry point returns a `Result`/`Option` (`AGENTS.md` §2.9).

## Tests

```
cargo test -p rustos-wm
```

Headless tests against a virtual framebuffer cover premultiplied-alpha
correctness (opaque and transparent edge cases), per-region alpha
blending, rounded-corner masking, z-order, window move/hide/remove with
damage repaint, channel-order encoding, the present seam, and input
routing (hit-testing, click-to-activate focus and raise,
desktop-clears-focus, move-grab drag, and the fail-closed grab edge
cases).
