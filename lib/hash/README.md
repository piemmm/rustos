# tairix-hash

Hashing for TAIRiX. Two hash functions and the key one of them is keyed with,
separated so the weaker choice cannot be made by accident:

| Type         | Kind                          | Use it for |
|--------------|-------------------------------|------------|
| `SipHash13`  | Keyed pseudo-random function  | Any hash over a key an attacker can choose or influence — filenames off a foreign volume, DNS names, 5-tuples, IPC method names, bundle ids, user-supplied futex addresses. |
| `FastHash`   | Fast, **not** keyed (XXH64)   | Kernel-assigned keys, content fingerprints, revision counters. |

`SipHash13` is the default. `FastHash` is opt-in by naming it, so a review can
see the choice.

## Why keyed

A hash table over attacker-chosen keys degenerates from O(1) to O(n) per
lookup once the attacker can predict which keys collide, and the keys TAIRiX
exposes to that are real. `SipHash13` (SipHash-1-3, Aumasson & Bernstein)
under a key the attacker cannot observe removes the attack; a fixed key does
not.

Neither function is cryptographic. `SipHash13` is a keyed PRF sized for
hash-table defence, not a MAC: message authentication, key derivation, and
digests are `lib/crypto`'s.

## The key

`HashSeed` is 128 bits drawn from the platform CSPRNG and published **once**
per boot in the kernel and **once per process** in userland, so no
cross-process collision oracle exists and a compromise of one process does not
reveal another's table layout. This crate never draws the key — it is
injected, which keeps the crate free of external dependencies and
host-testable, and leaves the boot path to decide where entropy comes from.

`published()` reports whether a key exists yet. A consumer whose hash is over
untrusted keys refuses to run unkeyed; a consumer whose hash is not a security
decision and must work before the CSPRNG is up names `HashSeed::UNKEYED`
explicitly. `HashSeed`'s `Debug` redacts the words, so a key cannot reach a log
through a derived `Debug` on an enclosing type.

## Tests

Unit tests live next to the code. Both implementations are pinned by published
vectors rather than by themselves:

- `SipHash13` against the published SipHash reference vector set (the
  incrementing prefixes under the reference key `00 01 … 0f`), cross-checked
  against two independent implementations before it was written down.
- `FastHash` against the XXH64 reference implementation's published outputs.

Beyond the vectors: streaming-versus-one-shot agreement over every chunking,
little-endian and pointer-width-independent integer writes, bucket-spread and
avalanche counters, and the one-shot publication semantics. `tests/fuzz_hash.rs`
drives both hashers over arbitrary bytes, lengths, and write splits under
`cargo xtask fuzz`.

## Stability

**experimental.** The public API may change until the first tagged release;
nothing here is frozen yet.
