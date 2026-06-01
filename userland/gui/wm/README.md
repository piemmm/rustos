# rustos-wm

The RustOS compositing window manager (`userland/gui/wm`, `AGENTS.md`
§10). It composes per-window surfaces into a single scan-out frame and
presents it through a capability-gated
`rustos_abi::driver::display::Display` driver. All compositing happens in
user space; no non-GUI crate depends on this crate (`AGENTS.md` §17.3).

## Status

First Stage 7 increment — the **compositor core**:

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

GPU acceleration, input routing, theming, and the taskbar build on this
core in later Stage 7 increments.

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
damage repaint, channel-order encoding, and the present seam.
