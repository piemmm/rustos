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
  `(face, glyph, cell height)` cache of already-rasterised coverage.
  `FontService::handle` is the whole request pipeline: decode one
  `tairix_abi::font_ipc::FontRequest`, resolve the scalar to its covering face
  (Inconsolata EX → M PLUS 1 Code → `D2Coding` → Noto Sans Hebrew → U+FFFD),
  rasterise once through the shared `lib/fontface` engine, and frame the reply.
  The face bytes are *injected* (borrowed), so the whole rasterise + cache path
  is unit-tested against the committed repository faces with no on-disk
  `/System/Fonts`.
- **`Run` (the program).** Reads the faces, builds the `FontService`, binds the
  endpoint, and serves. A pure-Rust freestanding program on the native Tier-1
  targets, an inert stub on the host.

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
- `CAP_LOG_EMIT` — its startup audit records.

Reading the world-readable `/System/Fonts` faces is a one-shot mode-gated open
(`AGENTS.md` §5.3), not a held capability. The service requests no spawn, no
network, and no filesystem *write* authority — the minimum-capability sandbox
of §19.5.

## Tests

`cargo test -p tairix-fontd --no-default-features` runs the host dispatcher
tests (geometry derivation, glyph round-trip, wide two-cell glyphs, U+FFFD
fallback, cache determinism, metrics scaling, malformed-request fail-closed,
and truncated-face construction failure). The TrueType parser the service
runs is fuzzed in `lib/fontface`; the request/reply framing is fuzzed in
`lib/abi` (`fuzz_decode`).
