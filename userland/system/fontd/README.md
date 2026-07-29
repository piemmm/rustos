# `tairix-fontd` — the sandboxed OS font service

Stability tier: **experimental**.

`fontd` is the single, sandboxed OS text-rasterisation service
(`AGENTS.md` §16.4, §19.5; `plans/FONT-SERVICE.md`). It is the *only*
process that holds a font face or runs the outline rasteriser: it loads the
four committed TrueType faces from `/System/Fonts/` once at startup, binds the
reserved `FONT_ENDPOINT`, and answers each request with the small 8-bit glyph
coverage bitmap the client blits — never a font byte. A malformed face can
therefore fault only this service's address space, never a compositor or a
terminal.

The installed binary lives at `/System/Services/fontd.app/Run` and is started
by `init` before the desktop. On a headless system it simply serves any text
client that asks; nothing requires it.

## What it is made of

- **`FontService` (the library, host-testable).** Owns the parsed
  [`FontFamily`](../../../lib/fontface) and a bounded
  `(face, glyph, cell height, weight)` cache of already-rasterised coverage.
  `FontService::handle` is the whole request pipeline: decode one
  `tairix_abi::font_ipc::FontRequest`, resolve the scalar to its covering face
  (Inconsolata EX → M PLUS 1 Code → `D2Coding` → Noto Sans Hebrew → U+FFFD),
  rasterise once through the shared `lib/fontface` engine, thicken the coverage
  to the requested weight, and frame the reply.
  The face bytes are *injected* (borrowed), so the whole rasterise + cache path
  is unit-tested against the committed repository faces with no on-disk
  `/System/Fonts`.
- **`Run` (the program).** Reads the faces, builds the `FontService`, binds the
  endpoint, and serves. A pure-Rust freestanding program on the native Tier-1
  targets, an inert stub on the host.
- **`embolden` (the weight transform).** The committed faces ship one weight
  each, so `Medium` (em/48) and `Bold` (em/24) are synthesised by thickening
  the rasterised 8-bit coverage *horizontally only*, in 1/256 px fixed point.
  That leaves the baseline, cell height, and pen advance untouched — a bold run
  occupies exactly the cells its regular twin would, so layout is
  weight-independent — and `Regular` adds a zero stroke, keeping body text
  byte-identical. The weight is part of the cache key, so each
  (glyph, size, weight) is emboldened once.

## Byte-identical rendering

The engine produces 4-bit coverage (`0..=15`), exactly as the console-atlas
generator does; each sample is scaled `×17` to the protocol's 8-bit sample
(`15 → 255`). A cell height's geometry scales the native cell the same way
`lib/font`'s blitter always has, so text drawn through the service is
byte-for-byte what the old in-process blitter produced.

## Capabilities

The manifest requests only:

- `CAP_IPC_BIND_PRIVILEGED` — to bind the reserved `FONT_ENDPOINT` (a squatter
  binding it first would feed forged coverage to every app).
- `CAP_FS_ACCESS` — the one-shot startup read of the four committed
  `/System/Fonts` faces through the secured VFS (which still authorises every
  path per-inode under the service's attested identity, and `/System` is
  mounted read-only so this reach can never write).
- `CAP_LOG_EMIT` — its startup audit records.

The service requests no spawn and no network authority, and the untrusted
TrueType parse runs in this service's own isolated address space — the
minimum-capability sandbox of `AGENTS.md` §19.5. It keeps no open descriptor
after the startup read.

## Tests

`cargo test -p tairix-fontd --no-default-features` runs the host dispatcher
tests (geometry derivation, glyph round-trip, wide two-cell glyphs, U+FFFD
fallback, cache determinism, metrics scaling, malformed-request fail-closed,
and truncated-face construction failure), the weight dispatcher tests (a
heavier weight inks more of the same cell; the weight keys the cache, so a
regular run is never served bold), and the emboldening unit tests (a zero
stroke leaves coverage byte-identical, ink never wraps into the next row, a
fractional stroke darkens the edge proportionally, coverage never exceeds full
opacity, a degenerate bitmap is left alone rather than indexed out of range,
the stroke scales with the rendered em, and an absurd stroke request is bounded
rather than wrapping). The TrueType parser the service runs is fuzzed in
`lib/fontface`; the request/reply framing is fuzzed in `lib/abi`
(`fuzz_decode`).
