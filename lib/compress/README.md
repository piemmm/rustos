# tairix-compress

The single shared **first-party compression codec** for TAIRiX (`AGENTS.md`
§6, §16.4 — compression is a curated shared-library class). It implements a
low-CPU, lossless LZ77 codec in the spirit of the zstd "fast" / LZ4 profiles
(`docs/src/filesystem/arxfs-spec.md` §10 — *the v1 target is a low-CPU
zstd-fast-style profile, not maximum ratio*), plus a **decode-only** RFC 1951
DEFLATE / RFC 1950 zlib reader used solely to read foreign compressed
formats — today, PNG's `IDAT` stream (`lib/image`).

- `compress(src, dst) -> Result<usize, Error>` — a single greedy LZ77 pass
  over a small hash table of recent 4-byte sequences. Writes into a
  caller-provided slice and returns the compressed length.
- `decompress(src, dst) -> Result<usize, Error>` — a tight literal-copy /
  match-copy loop. The declared output length is bounds-checked against `dst`
  before any byte is produced, and every back-reference is validated against
  the bytes produced so far.
- `max_compressed_len(input_len) -> usize` — a true upper bound on the
  compressed size, so a caller can size a scratch buffer that never provokes a
  spurious `Error::TooSmall`.

The wire frame is `"RLZ1"` magic, a little-endian `u32` uncompressed length,
then LZ4-style token sequences (a literal-run/match-length nibble token,
0xFF-continuation length extensions, the literal bytes, and a `u16`
back-reference offset). There is no entropy-coding stage, so the codec is fast
and predictable rather than maximally dense.

## Foreign-format interoperability: `inflate` and `zlib`

Two further modules are **decode-only** and exist purely to read a *foreign*
compressed format a PNG (or, in future, any other format whose encoder
chose DEFLATE) hands us — never to become a second TAIRiX-native codec:

- `inflate::inflate_into(src, dst) -> Result<usize, Error>` — a complete RFC
  1951 DEFLATE decompressor: stored, fixed-Huffman, and dynamic-Huffman
  blocks, canonical Huffman tables built by the reference count/offset walk
  (a real table-driven decode, not a linear scan over every code), and
  overlapping back-references copied byte-by-byte. Every alphabet in the RFC
  fits a fixed-size array, so decoding allocates nothing, matching the LZ
  codec above.
- `zlib::decompress_into(src, dst) -> Result<usize, Error>` — the RFC 1950
  envelope: header validation (compression method, window size, header
  check, refusing a preset dictionary), the wrapped `inflate` body, and the
  trailing Adler-32 verified over exactly the bytes produced.

There is deliberately no DEFLATE **compressor**: nothing in the tree produces
a DEFLATE stream, only foreign encoders do, so only the decode direction is
implemented. Both modules keep the crate's zero-`unsafe`, no-panic, fail-closed
discipline; see their own rustdoc for the full error taxonomy.

## Why it lives in `lib/`

ARXFS compresses every file-data record before encrypting it
(`docs/src/filesystem/arxfs-spec.md` §6, §10), and `AGENTS.md` §16.4 lists
compression among the curated OS-provided shared-library classes, so the codec
belongs in `lib/*` (§6) rather than buried in the filesystem driver. It is
written first-party because `AGENTS.md` §2.12 — *roll your own; do not trust
external code* — bars an external `zstd`/`lz4`/compression dependency. This is
**not** the crypto carve-out (§2.12): cryptography uses audited `lib/crypto`
primitives, but compression is ours.

The crate has no dependencies and sits at the bottom of the §17.4 layering: it
is depended on, never depends.

## Stability tier

`experimental` — the Stage 6 ARXFS compression seam
(`docs/src/filesystem/arxfs-spec.md` §15.6, §18). It is `no_std`, performs no
allocation (it works through caller-provided slices and fixed-size internal
tables, including in `inflate`/`zlib`), and has no dependencies. No `unsafe`,
and no `unwrap`/`expect`/`panic!` in production paths: every entry point is
`Result`-based and total, and malformed compressed input returns a typed
error rather than panicking (`AGENTS.md` §2.9). A future on-disk format
version may switch the codec globally; the `RLZ1` frame magic carries the
version.
