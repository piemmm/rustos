# `tairix-fontd` — the sandboxed OS font service

Stability tier: **experimental**.

`fontd` is the single, sandboxed OS text-rasterisation service
(`AGENTS.md` §16.4, §19.5; `plans/FONT-SERVICE.md`). It is the *only*
process that holds a font face or runs the outline rasteriser: it discovers the
`/System/Fonts` family store at startup, binds the reserved `FONT_ENDPOINT`,
and answers each request with the small 8-bit glyph coverage bitmap the client
blits — never a font byte. A malformed face can therefore fault only this
service's address space, never a compositor or a terminal.

The installed binary lives at `/System/Services/fontd.app/Run`. It is not a
boot-floor service: text rendering is a graphics-only resource, so `login`
starts it the first login round a machine is display-capable and a
headless/text-only boot never does (`plans/FONT-SERVICE.md` §3). Once started
it simply serves any text client that asks; nothing requires it.

## The store it serves from

`/System/Fonts` holds one directory per family, each carrying a `FontFamily`
manifest (parsed by `tairix_fontface::FamilyManifest`) that names the family's
label, its kind (`proportional`, `monospace`, or the coverage-only `fallback`
role), its faces in resolution order, and optionally one fallback family whose
faces extend its coverage. Nothing in this crate names a family or a face:
adding a family is dropping its directory into the store, and the image builder
plants exactly what the assets tree holds.

Discovery lists the store, reads each manifest, and skips — with a logged
warning, never fatally on its own — a directory whose name is not a valid
family key, that carries no readable manifest, or whose manifest does not
parse. A store with not one usable family *is* fatal: the service cannot serve
text at all, so it records `SERVICE_UNAVAILABLE` and exits rather than
answering with fabricated coverage.

**A face's bytes are read on first use, never at startup.** Discovery opens
each declared face read-only and keeps that handle; the bytes are read once,
the first time a request actually resolves to that face, and are then retained
for the service's life (the handle is released at that point). A session that
never draws Chinese therefore never pays for the Chinese face — which matters
on a small machine, where the shipped CJK companions are tens of megabytes.
The retained bytes are bounded by the store's own size, not by anything a
caller can grow.

## What it is made of

- **`FontService` (the library, host-testable).** Owns the discovered
  families, their lazily-loaded faces and per-weight parsed instances, and a
  byte-budgeted coverage cache (see *The cache a caller cannot grow* below).
  `FontService::handle` is the whole request pipeline: decode one
  `tairix_abi::font_ipc::FontRequest`, resolve, rasterise once through the
  shared `lib/fontface` engine (or serve the cached raster), and frame the
  reply.
- **`Run` (the program).** Discovers the store through `tairix-rt`, builds the
  glyph cache from the machine's RAM figure and this process's pressure gauge
  and audit sink, binds the endpoint, and serves from a wait set carrying both
  the endpoint and the kernel's memory-pressure source — so it reacts to a band
  change while idle and never polls for either. A pure-Rust freestanding
  program on the native Tier-1 targets, an inert stub on the host.
- **`discovery` (the store seam).** `FontStore` (list the store, read a
  manifest, hand back a per-face loader) and `FaceLoad` (produce one face's
  bytes on first use) are traits, so the whole discovery-to-serve pipeline is
  unit-tested against an in-memory fixture with no on-disk `/System/Fonts`.
- **`embolden` (the synthetic-weight transform).** Only for a face with no
  `wght` axis: the rasterised 8-bit coverage is thickened *horizontally only*,
  in 1/256 px fixed point, leaving the baseline, box height, and pen advance
  untouched — so a synthetically bold run occupies exactly what its regular
  twin would. `Regular` adds a zero stroke, keeping body text byte-identical.

## Resolution

A `Glyph` request resolves its scalar in one deterministic order, and never
refuses it for lack of coverage:

1. the requested family's own faces, in manifest order — the first whose
   `cmap` maps the scalar wins;
2. then, if the family declares one, its fallback family's faces, in the same
   order;
3. else U+FFFD, rasterised from the requested family's primary face.

Every glyph in one family's run is rasterised at the geometry that family's
**primary** face defines at the requested pixel height — pixels per em,
baseline row, and box height — even when the glyph itself came from a fallback
face, so mixing scripts never shifts the baseline or the line box mid-run.

An unknown family key is refused with `Errno::NotFound`. There is no
substitution: a client that asks for a family the machine does not have gets an
error, not silently different text.

`Metrics` answers from the family's primary face: the echoed pixel height, the
baseline row, the line height (the pixel height plus the face's own line gap),
and a `monospace_advance` that is non-zero **only** when the manifest says
`monospace` *and* the face really does advance uniformly — a proportional
family always reports `0`, which is how a client learns it must ask per glyph.

`Families` lists the selectable families only, in discovery order (sorted by
directory name, so it does not depend on how the filesystem happens to list
them). A `fallback`-role family is never offered: it exists to extend another
family's coverage, not to be chosen.

## Weights

A face declaring a `wght` variation axis is instanced at the requested weight's
real OpenType coordinate (400 / 500 / 700), so a bold glyph is the weight the
designer drew and its advance genuinely differs from the regular one. Each
distinct weight actually asked for is parsed once per face and kept, so no
request re-parses a face.

A face with no `wght` axis has one design instance, and the requested weight is
synthesised by `embolden` instead — which never moves the advance, so a client
laying out with the regular metrics is still correct.

## Byte-identical rendering

The engine produces 4-bit coverage (`0..=15`), exactly as the console-atlas
generator does; each sample is scaled `×17` to the protocol's 8-bit sample
(`15 → 255`). One engine rasterises both the compiled-in console atlas and
every glyph this service serves, so there is no second rasteriser to drift.

## The cache a caller cannot grow

The requested pixel height comes from the caller, so the *size* of what a
request makes this service retain is caller-influenced: a client walking the
permitted height range at the widest permitted bitmap would drive an
entry-counted cache into hundreds of megabytes. The cache is therefore bounded
in **bytes**, by a budget derived from the machine's total RAM, through the
shared reclaimable-memory model
([`tairix_reclaim::ReclaimCache`](../../../lib/reclaim)) built from the one
cached-glyph declaration in [`lib/font`](../../../lib/font)'s `glyph_cache`
module — the same declaration the render-path client on the other side of the
endpoint uses, so the two cannot drift apart (`AGENTS.md` §2.2). However many
distinct sizes a caller asks for, the retained bytes stay under that ceiling
and the least recently used rasters are released and overwritten to make room.

The key names the requesting family, the family whose face actually supplied
the glyph, that face's index, the glyph id, the pixel height, and the weight.
Both families are in the key because two families sharing one fallback face
compute their geometry from their own primary faces, so the very same physical
glyph can legitimately rasterise to two different bitmaps — keying by the
resolved face alone could serve one family's raster to the other.

Bounding retention is not input validation and does not replace it: the
permitted scalar, pixel-height, and weight ranges are checked by the
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
- `CAP_FS_ACCESS` — the startup scan of the `/System/Fonts` family manifests
  and the first-use read of each face, through the secured VFS (which still
  authorises every path per-inode under the service's attested identity, and
  `/System` is mounted read-only so this reach can never write).
- `CAP_LOG_EMIT` — its startup and skipped-family audit records.

The service requests no spawn and no network authority, and the untrusted
TrueType parse runs in this service's own isolated address space — the
minimum-capability sandbox of `AGENTS.md` §19.5. The only descriptors it holds
are the read-only face handles opened at discovery, each released as soon as
that face's bytes have been read.

## Tests

`cargo test -p tairix-fontd` runs the host tests:

- **Discovery** — an empty store is a fatal startup error; an invalid family
  key, a missing manifest, and a malformed manifest are each skipped without
  killing the scan; discovery order does not depend on the store's listing
  order; a `fallback`-role family is discovered but never offered.
- **Resolution** — a scalar resolves from the primary face, from a later face
  in the family's own list, through the fallback family, and finally to U+FFFD
  when nothing maps it; an unknown family key fails closed for both `Glyph`
  and `Metrics`.
- **Metrics** — a monospace family reports its uniform advance, a proportional
  family reports `0`, and both echo the requested pixel height.
- **Weights** — a real `wght` axis changes the reported advance and inks more
  of the glyph; a static face's synthetic bold inks more while leaving the
  advance, width, and height untouched.
- **Cache** — a second identical request serves the same bytes; two families
  sharing a fallback face key separately and a repeat request does not grow the
  cache; mild memory pressure empties it and refuses further growth while the
  band forbids it; the cache reports under a wire-renderable label.
- **Framing** — an out-of-range pixel height and a corrupt request frame are
  both refused with an error frame.
- **Emboldening** — a zero stroke leaves coverage byte-identical, ink never
  wraps into the next row, a fractional stroke darkens the edge
  proportionally, coverage never exceeds full opacity, a degenerate bitmap is
  left alone rather than indexed out of range, and the stroke scales with the
  rendered em.

Fixtures are built from the small committed faces (`mono/Inconsolata-EX.ttf`,
`inter/Inter-Variable.ttf`, `mono/NotoSansHebrew-ExtraCondensed.ttf`) through
the in-memory store, so no test embeds a multi-megabyte CJK face. The TrueType
parser the service runs is fuzzed in `lib/fontface`; the request/reply framing
is fuzzed in `lib/abi` (`fuzz_decode`).
