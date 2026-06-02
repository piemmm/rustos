# rustos-compress

The single shared **first-party compression codec** for RustOS (`AGENTS.md`
§6, §16.4 — compression is a curated shared-library class). It implements a
low-CPU, lossless LZ77 codec in the spirit of the zstd "fast" / LZ4 profiles
(`docs/src/filesystem/rustfs-spec.md` §10 — *the v1 target is a low-CPU
zstd-fast-style profile, not maximum ratio*).

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

## Why it lives in `lib/`

RustFS compresses every file-data record before encrypting it
(`docs/src/filesystem/rustfs-spec.md` §6, §10), and `AGENTS.md` §16.4 lists
compression among the curated OS-provided shared-library classes, so the codec
belongs in `lib/*` (§6) rather than buried in the filesystem driver. It is
written first-party because `AGENTS.md` §2.12 — *roll your own; do not trust
external code* — bars an external `zstd`/`lz4`/compression dependency. This is
**not** the crypto carve-out (§2.12): cryptography uses audited `lib/crypto`
primitives, but compression is ours.

The crate has no dependencies and sits at the bottom of the §17.4 layering: it
is depended on, never depends.

## Stability tier

`experimental` — the Stage 6 RustFS compression seam
(`docs/src/filesystem/rustfs-spec.md` §15.6, §18). It is `no_std`, performs no
allocation (it works through caller-provided slices), and has no dependencies.
No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths: both entry
points are `Result`-based and total, and malformed compressed input returns
`Error::Corrupt` rather than panicking (`AGENTS.md` §2.9). A future on-disk
format version may switch the codec globally; the `RLZ1` frame magic carries
the version.
