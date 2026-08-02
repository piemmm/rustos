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

The installed binary lives at `/System/Services/fontd.app/Run`. It is not a
boot-floor service: text rendering is a graphics-only resource, so `login`
starts it the first login round a machine is display-capable and a
headless/text-only boot never does (`plans/FONT-SERVICE.md` §3). Once started
it simply serves any text client that asks; nothing requires it.

## What it is made of

- **`FontService` (the library, host-testable).** Owns the parsed
  [`FontFamily`](../../../lib/fontface) and a byte-budgeted
  `(face, glyph, cell height, weight)` cache of already-rasterised coverage
  (see *The cache a caller cannot grow* below).
  `FontService::handle` is the whole request pipeline: decode one
  `tairix_abi::font_ipc::FontRequest`, resolve the scalar to its covering face
  (Inconsolata EX → M PLUS 1 Code → `D2Coding` → Noto Sans Hebrew → U+FFFD),
  rasterise once through the shared `lib/fontface` engine, thicken the coverage
  to the requested weight, and frame the reply.
  The face bytes are *injected* (borrowed), so the whole rasterise + cache path
  is unit-tested against the committed repository faces with no on-disk
  `/System/Fonts`.
- **`Run` (the program).** Reads the faces, builds the glyph cache from the
  machine's RAM figure and this process's pressure gauge and audit sink,
  builds the `FontService`, binds the endpoint, and serves from a wait set
  carrying both the endpoint and the kernel's memory-pressure source — so it
  reacts to a band change while idle and never polls for either. A pure-Rust
  freestanding program on the native Tier-1 targets, an inert stub on the
  host.
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

## The cache a caller cannot grow

The requested cell height comes from the caller, so the *size* of what a
request makes this service retain is caller-influenced: a client walking the
permitted height range at the widest permitted bitmap (512 KiB apiece) would
drive an entry-counted cache into hundreds of megabytes. The cache is
therefore bounded in **bytes**, by a budget derived from the machine's total
RAM, through the shared reclaimable-memory model
([`tairix_reclaim::ReclaimCache`](../../../lib/reclaim)) built from the one
cached-glyph declaration in [`lib/font`](../../../lib/font)'s `glyph_cache`
module — the same declaration the render-path client on the other side of the
endpoint uses, so the two cannot drift apart (`AGENTS.md` §2.2). However many
distinct sizes a caller asks for, the retained bytes stay under that ceiling
and the least recently used rasters are released and overwritten to make room.

Bounding retention is not input validation and does not replace it: the
permitted scalar, cell-height, and weight ranges are checked by the
`tairix_abi::font_ipc` wire decode before a request reaches the dispatcher,
and an out-of-range request is refused with an error frame rather than
rasterised.

The cache is injected at construction (`tairix_fontd::glyph_cache` assembles
one), because sizing it needs the RAM figure and governing it needs the
process's own gauge and sink. A RAM reading the System Information service
cannot supply is zero, which yields a zero budget that retains nothing — every
glyph is then rasterised on demand: correct, merely slower, never a
hand-picked ceiling standing in for a figure the machine did not supply.

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
and truncated-face construction failure), the cache-bound tests (a caller
walking the top of the permitted size range never pushes the service past its
byte ceiling; an out-of-range size is still refused and rasterises nothing; a
zero RAM figure retains nothing yet still serves; mild pressure empties the
cache and refuses further growth; a glyph rasterises identically cached,
uncached, and after a forced shrink), the weight dispatcher tests (a
heavier weight inks more of the same cell; the weight keys the cache, so a
regular run is never served bold), and the emboldening unit tests (a zero
stroke leaves coverage byte-identical, ink never wraps into the next row, a
fractional stroke darkens the edge proportionally, coverage never exceeds full
opacity, a degenerate bitmap is left alone rather than indexed out of range,
the stroke scales with the rendered em, and an absurd stroke request is bounded
rather than wrapping). The TrueType parser the service runs is fuzzed in
`lib/fontface`; the request/reply framing is fuzzed in `lib/abi`
(`fuzz_decode`).
