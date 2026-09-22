# `tairix-compress`

The one home for lossless compression in TAIRiX. It holds two codecs that do
not compete: the first-party `RLZ1` codec TAIRiX's own storage uses, and the
RFC 1951 / RFC 1950 DEFLATE and zlib codec that foreign software already
speaks.

The crate depends on nothing — not even `alloc` — and allocates nothing. Its
consumers reach from the kernel heap's compressed-page store down through a
filesystem driver to a desktop image decoder, so a heap requirement would be
one imposed on all of them by the least constrained.

## Why two codecs

`RLZ1` exists because `AGENTS.md` §2.12 bars an external `zstd`/`lz4`
dependency and ARXFS needs a low-CPU codec on its record path
(`docs/src/filesystem/arxfs-spec.md` §10). It is a single greedy LZ77 pass
over a small hash table, with no entropy stage: fast and predictable rather
than dense.

DEFLATE exists because two foreign contracts are written in it and neither
is negotiable. A PNG's `IDAT` stream is zlib-wrapped DEFLATE, so `lib/image`
must *read* it. SSH's `zlib@openssh.com` compresses a session's traffic as
one zlib stream per direction, so `sshd` and `ssh` must *write* one a stock
OpenSSH peer reads (`plans/SSH.md`). Neither codec is a fallback for the
other: a TAIRiX-native format never stores DEFLATE, and a foreign format is
never handed `RLZ1`.

## The two shapes, and why both exist

| Shape | Entry point | For |
|---|---|---|
| Whole stream | `compress` / `decompress`, `inflate::inflate_into`, `zlib::decompress_into` | data that is present in one buffer: a filesystem record, a PNG `IDAT` |
| Streaming | `deflate::Deflate`, `inflate::Inflater`, `zlib::Encoder`, `zlib::Decoder` | a conversation: one stream per direction, flushed per message |

A one-shot function cannot express what a protocol needs. `zlib@openssh.com`
keeps a single zlib stream alive for the whole session and flushes it at each
packet boundary, so packet ten still back-references packet three. Restarting
the codec per packet would lose most of the ratio the feature exists for; and
on the receiving side, a fragment of a continuous stream is not a stream at
all — it has no final block and its back-references point behind its own
first byte.

So the streaming shape is a value the caller owns:

```rust,ignore
let mut encoder = Box::new(zlib::Encoder::new());
let mut wire = vec![0u8; encoder.bound(payload.len())];
let written = encoder.compress(payload, &mut wire, zlib::Flush::Sync)?;
```

`Flush::Sync` ends the current block and appends an empty stored block, so
the peer can decode every byte fed so far. `Flush::Finish` ends the stream.
`Flush::None` buffers, which is what bulk data wants.

## Memory, and where it goes

A decoder carries the 32 KiB of history a back-reference may reach into
(~34 KiB all told). An encoder additionally carries the match finder: a
64 KiB buffer the 32 KiB window slides inside, the hash head and chain
tables, and one block's tokens — around 220 KiB.

The 32 KiB window is the format's own, fixed by RFC 1951 and by what a peer
expects (`AGENTS.md` §24.4). The hash and token table sizes are tuning, and
are documented on their constants.

That memory is deliberately *not* hidden. There is no one-shot encode
function, because one would put 220 KiB on a caller's stack without saying
so; the caller allocates the state and decides where it lives. The one-shot
*decode* functions remain, because the machine they run needs no window at
all when the whole output stays addressable in the caller's buffer — so a
PNG decode pays neither the window nor a copy into it.

## Output sizing, and why it is a bound rather than a retry

The encoder writes straight into the caller's slice and cannot rewind, so
the caller sizes the destination with `bound(input_len)` and a shorter one is
refused before any state changes. This is the discipline
`max_compressed_len` already set for `RLZ1`, applied to a stream.

The bound holds because a block is never emitted larger than the bytes it
covers plus its header. A block's input span is capped below what a stored
block can carry, so the stored form is always available as a floor, and each
block goes out as whichever of stored, fixed-Huffman, and dynamic-Huffman is
smallest. Incompressible input therefore costs a header per block rather
than the eighth a Huffman-only encoder would add.

## Where it stands against zlib

Measured on one corpus against the reference implementation, so the ratios
rather than the absolute rates are the point.

The encoder produces zlib-level-6 output — within 0.2% on the same corpus —
at rather better than level 6's speed, which is where a full-window match
finder with a bounded chain walk and lazy matching should land.

The decoder is the other way round: roughly seven times slower than zlib's,
because it walks a canonical Huffman code one bit at a time where zlib reads
a multi-level lookup table. That is the strategy this module has always used
and it is what keeps every alphabet a plain fixed-size array. Closing it
means adding a derived lookup table for short codes, which would speed bulk
decoding several-fold and *slow* the small-block case a packetised protocol
lives in, since the table is rebuilt per block. Which of those matters is a
question for a profile of a real consumer, not for a guess, so the number is
recorded here rather than acted on.

## Robustness

Every entry point is `Result`-based and total. The crate is `no_std`, has no
dependencies, contains no `unsafe`, and has no `unwrap`/`expect`/`panic!` on
a production path: a malformed, truncated, or adversarial stream returns a
typed error. Output bounds are checked before any byte is produced.

Both decode directions are attacker-reachable — a PNG from anywhere, an SSH
peer's traffic — so both carry a fuzz harness in `cargo xtask fuzz`
(`fuzz_inflate`, `fuzz_zlib`), alongside the `RLZ1` decoder's `fuzz_compress`.

## Proven against the real thing

A codec tested only against itself agrees with its own bugs, so the DEFLATE
tests are pinned to streams a real zlib produced — fixed-Huffman,
dynamic-Huffman, stored, a 20 KiB-distance match, and an OpenSSH-shaped
sequence of `Z_PARTIAL_FLUSH` frames that each end mid-byte. The encode
direction was likewise verified by inflating its output under a real zlib.
The permanent in-tree gate is the fixture set plus the round-trip harnesses;
the end-to-end check against a pinned OpenSSH is `plans/SSH.md` S13.

## Stability tier

`experimental`. See the crate's `README.md`.
