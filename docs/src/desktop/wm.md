# Compositing window manager

`userland/gui/wm` (`rustos-wm`) is the user-space compositor for the
RustOS desktop (`AGENTS.md` §10). All compositing happens in user space;
the kernel only ships framebuffer access through a capability, and no
non-GUI crate depends on it (`AGENTS.md` §17.3). This page documents the
**compositor core** delivered as the first Stage 7 increment; GPU
acceleration, input routing, theming, and the taskbar build on it in
later increments.

## Pipeline

The compositor turns a stack of windows into one scan-out frame:

1. Each window owns a `Surface`: a dense, row-major buffer of
   **premultiplied-alpha** `Pixel`s.
2. `Compositor::composite` walks the damaged screen regions and, for
   every dirty pixel, blends each covering window *over* the opaque
   background, bottom-to-top in z-order, using the Porter–Duff *over*
   operator (`Pixel::over`).
3. Each composited pixel is encoded into a byte frame laid out for the
   active `DisplayMode` (`Rgba8888` or `Bgra8888`).
4. `Compositor::present` hands that frame to a `Display` driver.

Because the root background is forced opaque, the final screen is always
fully opaque and its premultiplied channels equal their straight-alpha
form on scan-out.

## Premultiplied alpha

Working in premultiplied alpha makes the *over* operator a single
multiply-add per channel and keeps per-surface, per-region, and
rounded-corner coverage correct:

- `Color::premultiply` converts an authored straight-alpha colour into a
  stored `Pixel`.
- `Pixel::scale_alpha` applies an opacity factor (per-window opacity ×
  corner coverage) by scaling every channel — colour and alpha — at once.
- `Pixel::over` composites a premultiplied source over a premultiplied
  destination as `src + dst * (1 - src.a)`.

## Rounded corners

`Corners::Rounded { radius }` rounds a window's corners; `Corners::Square`
is the opt-out. Coverage in `0..=255` is computed by deterministic
supersampling on a fixed grid (no `sqrt`, which `core` lacks), so a pixel
on a corner arc receives anti-aliased partial coverage and the
anti-aliasing is exactly reproducible in tests. The radius is clamped to
half the shorter side. The taskbar's rounded edges reuse this same path
rather than a second implementation (`AGENTS.md` §2.2).

## Damage tracking

`DamageRegion` records the screen rectangles that changed since the last
frame (a window was added, moved, restyled, hidden, raised, or removed).
`Compositor::composite` recomputes only those pixels and then clears the
damage, so an idle desktop costs nothing to recomposite.

## Failing closed

Every fallible entry point returns a `Result`/`Option` rather than
panicking (`AGENTS.md` §2.9): `Compositor::new` and `Surface::new` return
`None` for a surface too large to allocate or a stride too small for one
scanline, and an unsupported pixel format is refused at construction
rather than guessed (`AGENTS.md` §2.1). There is no `unsafe` in the
crate.

## Tests

`cargo test -p rustos-wm` runs the headless suite against a virtual
framebuffer: premultiplied-alpha correctness (fully-opaque and
fully-transparent edge cases), per-region alpha blending, rounded-corner
masking, z-order and raise, window move/hide/remove with damage repaint,
channel-order encoding, and the `Display` present seam.
