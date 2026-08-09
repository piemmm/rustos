# Font service (`userland/system/fontd`)

`tairix-fontd` is the user-space service that owns the system's fonts and
renders text. Text rendering is a single, sandboxed OS resource (`AGENTS.md`
§16.4, §19.5): `fontd` is the **only** process that holds a font face or runs
the TrueType outline rasteriser. Every other process — the compositor, the
taskbar, a terminal, an app — draws text by asking `fontd` for a glyph's
coverage over the reserved `FONT_ENDPOINT`, through the thin
[`tairix-font`](../lib/font.md) client. The installed binary lives at
`/System/Services/fontd.app/Run`.

The crate is `no_std` and depends only on the audited `lib/*` crates
`tairix-abi`, `tairix-fontface`, `tairix-log`, `tairix-reclaim`, and
`tairix-font`'s shared cached-glyph declaration, so a userland service never
links a kernel or driver crate (`AGENTS.md` §17.4).

## Why a service

Older builds embedded the full-Unicode glyph atlas (~3.6 MB) and the four
TrueType faces (~6.1 MB) into **every** GUI consumer via `include_bytes!`, so
each `Run` image carried its own ~10 MB read-only copy. Launching a GUI app
read, hashed, and eagerly copied all ~10 MB, which was the reported slow
desktop launch — glacial under QEMU TCG. Moving the payload to one service
removes it from every app image and, at the same time, satisfies four charter
rules the old stack violated: fonts as a curated OS shared library (§16.4),
system fonts under `/System/Fonts` (§16.2), untrusted font parsing in a
minimum-capability sandbox (§19.5), and no data duplicated into every consumer
(§2.2, §2.3).

## The sandbox and its authority

`fontd` runs in its own address space as the dedicated `fontd` service account
(uid 15, primary group `services`). Its signed `AppInfo` requests exactly three
capabilities and its account ceiling grants exactly the same three:

- `CAP_IPC_BIND_PRIVILEGED` — to bind the reserved well-known `FONT_ENDPOINT`,
  so a squatter cannot claim the rendezvous first and feed forged glyph
  coverage to the compositor and every app.
- `CAP_FS_ACCESS` — for the startup scan of the `/System/Fonts` family
  manifests and the first-use read of each face, through the secured VFS,
  which still authorises every path per-inode under the service's attested
  identity. `/System` is mounted read-only, so this reach can never write.
- `CAP_LOG_EMIT` — for its structured audit records (the `17000` event range:
  `SERVICE_READY`, `SERVICE_UNAVAILABLE`, `FAMILY_SKIPPED`).

It requests **no** spawn or network authority. The only descriptors it holds
are the read-only face handles opened while scanning the store, each released
as soon as that face's bytes have been read; from then on the service retains
only the in-memory faces. The untrusted TrueType parse runs in this service's
own isolated address space, so even a malformed face — the classic font-parser
attack surface — faults only this sandbox, never a compositor or a terminal.
Serving glyph coverage needs no capability of its own: drawing text is not a
security boundary (§5.2), and the *reply path* still validates every field and
fails closed on a corrupt frame (§5.4).

## The store, discovered not hardcoded

`/System/Fonts` holds one directory per family, each carrying a `FontFamily`
manifest (`tairix_fontface::FamilyManifest`) naming the family's label, its
kind (`proportional`, `monospace`, or the coverage-only `fallback` role), its
faces in resolution order, and optionally one fallback family whose faces
extend its coverage. Nothing in the service names a family or a face: adding a
family is dropping its directory into the store (the image builder plants
exactly what `lib/font/assets/` holds), so the shipped set — `inter`,
`noto-sans`, `noto-serif`, the `mono` console family, and the shared
`sans-fallback` coverage set — is data, not code.

Discovery is bounded to `FONT_MAX_FAMILIES` directories and sorted by
directory name, so what the service offers does not depend on how the
filesystem happens to list the store. A directory whose name is not a valid
family key, that carries no readable manifest, or whose manifest does not parse
is **skipped with a logged warning** (`FAMILY_SKIPPED`) — one bad family never
takes the store down. A store with not one usable family *is* fatal: the
service records `SERVICE_UNAVAILABLE` and exits rather than serving fabricated
coverage.

**A face's bytes are read on first use, never at startup.** Discovery opens
each declared face read-only and keeps the handle; the bytes are read once, the
first time a request actually resolves to that face, and are retained for the
service's life. A session that never draws Chinese never pays for the Chinese
face — which matters on a small machine, where the shipped CJK companions are
tens of megabytes. What is retained is bounded by the store's own size, not by
anything a caller can grow.

## The dispatcher

The host-testable core is `FontService`, the rasterising dispatcher. It owns
the discovered families, their lazily-loaded faces and per-weight parsed
instances, and a byte-budgeted coverage cache, and turns one decoded
`FontRequest` into a framed reply:

1. Resolve the requested scalar within the named family: its own faces in
   manifest order, then its declared fallback family's faces in the same
   order, then U+FFFD from the family's primary face. A scalar is never
   refused for lack of coverage; an *unknown family key* is refused with
   `Errno::NotFound` and never silently substituted.
2. Rasterise once at the geometry the family's **primary** face defines at the
   requested pixel height — pixels per em, baseline row, box height — even
   when the glyph came from a fallback face, so mixing scripts never shifts
   the baseline or the line box mid-run. The shared `tairix-fontface` engine
   produces 4-bit coverage scaled ×17 to the protocol's 8-bit samples, and
   the result is memoised. The engine grid-fits the outline along **rows
   only** here: snapping rows holds a run's baseline, x-height and cap height
   to whole pixels, while columns stay exactly where the outline puts them so
   the ink still sits under the unfitted advance `Metrics` reported.
3. Emit the reply. `handle` **always** emits a reply, framing a status-word
   error frame on any failure so both the glyph and metrics clients decode a
   definite outcome (fail closed).

`Metrics` answers from the family's primary face: the echoed pixel height, the
baseline row, the line height (pixel height plus the face's own line gap), and
a `monospace_advance` that is non-zero **only** when the manifest says
`monospace` *and* the face really does advance uniformly — a proportional
family always reports `0`, which is how a client learns it must ask per glyph.
`Families` lists the selectable families only; a `fallback`-role family exists
to extend another's coverage and is never offered to a user.

The store is reached through two injected seams (`FontStore` for the scan,
`FaceLoad` for one face's bytes), so the whole discovery-to-serve pipeline is
exhaustively host-tested against an in-memory fixture built from the small
committed faces — no on-disk `/System/Fonts`, and no multi-megabyte face in a
test binary. The `tairix-fontface` TrueType parser additionally carries its own
fuzz harness (`AGENTS.md` §19.6).

## What a caller can make the service hold

The requested pixel height comes from the caller, so the *size* of what a
request makes the service retain is caller-influenced: a client walking the
permitted height range at the widest permitted bitmap would drive an
entry-counted cache into hundreds of megabytes. The cache is therefore bounded
in **bytes**, by a budget derived from the machine's total RAM
(`tairix_procinfo::memory_total_bytes`), through the shared
`tairix_reclaim::ReclaimCache` — the very cache, and the very cached-glyph
declaration (`tairix_font::glyph_cache`), the render-path client on the other
side of the endpoint uses, so the two cannot drift apart. However many
distinct sizes a caller asks for, the retained bytes stay under that ceiling
and the least recently used rasters are released and overwritten to make room.

The key names the **requesting** family, the family whose face actually
supplied the glyph, that face's index, the glyph id, the pixel height, and the
weight. Both families are in the key because two families sharing one fallback
face derive their geometry from their own primary faces, so the very same
physical glyph can legitimately rasterise to two different bitmaps — keying by
the resolved face alone could serve one family's raster to the other.

Bounding retention is not input validation and does not replace it: the
permitted scalar, pixel-height, and weight ranges are checked by the
`tairix_abi::font_ipc` wire decode before a request reaches the dispatcher,
and an out-of-range request is refused with an error frame rather than
rasterised.

The cache is injected at construction, since sizing it needs the RAM figure
and governing it needs the process's own pressure gauge and audit sink:
`tairix_fontd::glyph_cache` assembles one, and the `Run` binary supplies all
three. A RAM reading the System Information service cannot supply is zero,
which yields a zero budget that retains nothing — every glyph is then
rasterised on demand, correct and merely slower, never a hand-picked ceiling
standing in for a figure the machine did not supply.

The serve loop waits on a wait set carrying both the endpoint and the
kernel's memory-pressure source, so the service reacts to a band change while
idle — giving its rasters back as the machine tightens — without ever polling
for either.

## The `FONT_ENDPOINT` protocol

The wire protocol lives in `tairix_abi::font_ipc`, modelled on the other
reserved-endpoint service protocols (`display_ipc`, `window_ipc`) and held to
the same ABI discipline as the syscall table (§9): versioned, hashed, and
frozen on the first release (mutable now — `abi-v1` is not frozen). It is not
part of the curated C-ABI surface, so the generated C headers are unchanged.

- A fixed 36-byte `FontRequest` in: `Glyph { family, scalar, pixel_height,
  weight }`, `Metrics { family, pixel_height, weight }`, or `Families`. The
  family is a validated `FamilyKey` (a `FONT_FAMILY_KEY_LEN`-byte lower-case
  key, so a path separator or a stray byte is unrepresentable in an accepted
  request); the scalar is carried as a `char`, so a surrogate or out-of-range
  code point is likewise unrepresentable; the pixel height is bounded by
  `FONT_MIN_PIXEL_HEIGHT`/`FONT_MAX_PIXEL_HEIGHT` (8..=512) — a validation
  bound, not a capacity. The `weight` is a closed `FontWeight` (`Regular`,
  `Medium`, `Bold`) decoded from its wire value, so an unknown weight is
  refused rather than coerced. Every field an operation does not use, and the
  reserved halfword, must be zero, so a smuggled field is a decode failure,
  never silently ignored.
- A status-framed reply out: a glyph reply is `width`, `height`, `advance`,
  `left`, and the `width * height` 8-bit coverage samples (bounded by
  `FONT_MAX_GLYPH_REPLY`; `width == 0` is an ink-less glyph such as a space);
  a metrics reply is `FontMetrics { pixel_height, baseline, line_height,
  monospace_advance }`, where a `monospace_advance` of `0` *means*
  proportional; a families reply is up to `FONT_MAX_FAMILIES` entries of
  (key, label, kind). One shared `glyph_coverage_len` bounds check governs both
  encode and decode, so producer and consumer cannot diverge.

## Weights: real where the face has an axis, synthetic where it does not

A theme names a weight per text role (see [Desktop theming](../desktop/theming.md)).
The shipped proportional families are **variable** faces, so a weight is a real
design instance: the face is instanced at the requested weight's OpenType
`wght` coordinate (400 / 500 / 700), which changes the outlines *and* the
advances the service reports — a bold run is genuinely wider than its regular
twin, exactly as the designer drew it. Each distinct weight actually asked for
is parsed once per face and kept, so no request re-parses a face.

A face with no `wght` axis — the static console faces — has one design
instance, and the requested weight is synthesised instead: `Medium` adds a
stroke of em/48 and `Bold` em/24, carried in 1/256 px fixed point so the
thickening is a smooth function of the rendered size rather than a whole-pixel
jump. Two properties make that safe to put on the text path:

- **The stroke is horizontal only.** A vertical smear would push an ascender or
  descender out of the box the client laid out, contradicting the geometry
  `FontMetrics` promised. A horizontal one stays inside the bitmap and leaves
  the baseline, box height, and pen advance untouched, so a synthetically bold
  run occupies exactly what its regular twin would.
- **It transforms coverage, not outlines.** Thickening the 8-bit alpha samples
  keeps the whole operation inside the sandbox that already owns the raster,
  needs no second rasterisation pass, and cannot move a control point. A
  `Regular` request adds a stroke of zero and is byte-identical to the
  pre-weight output.

The weight is part of the service's cache key, so each
(family, glyph, size, weight) is rendered once and the hot path is a cache read.

## Startup and discovery

`fontd` ships as a signed `/System/Services/fontd.app` bundle — a service is an
app (§16.2, §16.5). It is **not** a boot-floor service: text rendering is only
needed by the graphical desktop, so a headless or text-only system never runs
it (headless-first, §17.3). Instead **`login` starts it** the first login round
a machine is display-capable (the desktop bundle is installed and a display
service is live). login is the natural owner: it holds `CAP_SPAWN_AS_USER` —
the authority the graphics-only `fontd` account (uid 15) needs and that neither
the shell nor the desktop app has — so it drops `fontd` onto its own service
account exactly as it drops a session onto the authenticated user. This covers
both ways the desktop is launched (a graphical login, or the shell's `desktop`
command) and starts `fontd` once per login process; a duplicate would fail
closed on the reserved-endpoint bind. A refused start is audited
(`FONTD_UNAVAILABLE`) and login proceeds — the desktop degrades to unrendered
text rather than failing (§2.24).

`login` spawns `fontd` by its path, which the kernel resolves through the same
program gate as any other program: from the verified on-disk
`/System/Services/fontd.app` bundle on the aarch64 production build, and from
the compiled-in program registry on x86_64/riscv64 until those ports' on-disk
storage floors land (`fontd` is a registered spawnable program on those ports,
not an init-auto-started boot service). The desktop's font client fails closed
until `fontd` has bound `FONT_ENDPOINT`, so the first frames may paint no text
and then fill in once the service is serving.

> Note: starting `fontd` from the post-boot graphical path (rather than as an
> init boot service) is the headless-first-correct design in its own right — a
> text-only or headless system never needs a font renderer. An earlier concern
> that a 5th concurrent boot service crashed the kernel (D18 in
> `plans/OPEN-DEFECTS.md`) was investigated and found non-reproducing once this
> service's ~10 MB payload was removed; the design choice stands on
> headless-first alone.
