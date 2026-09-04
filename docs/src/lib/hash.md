# `tairix-hash`

Two hash functions and the key one of them is keyed with. `no_std`, no
external dependencies, allocation-free, and no `unsafe`.

| Type        | Kind                         | Use it for |
|-------------|------------------------------|------------|
| `SipHash13` | Keyed pseudo-random function | Any hash over a key an attacker can choose or influence. |
| `FastHash`  | Fast, **not** keyed (XXH64)  | Kernel-assigned keys, content fingerprints, revision counters. |

`SipHash13` is the default. `FastHash` is opt-in by naming it, so the weaker
choice is visible in review.

## Why the split

A hash table over attacker-chosen keys degenerates from O(1) to O(n) per
lookup once the attacker can predict which keys collide, and the keys TAIRiX
exposes to that are real: filenames from a mounted foreign volume, DNS names,
network 5-tuples, IPC method names, bundle identifiers, and the user-supplied
addresses a process waits on with the futex. A keyed pseudo-random function
under a key the attacker cannot observe removes the attack; a fixed key does
not.

Where nothing hostile chooses the input — a self-verify fingerprint over the
crate's own scratch, a revision counter over this host's own configured
addresses, a build-provenance id — the fast unkeyed hash is the right tool and
the key would buy nothing.

Neither function is cryptographic. `SipHash13` is a keyed PRF sized for
hash-table defence, not a message-authentication code: MACs, key derivation,
and digests are [`tairix-crypto`](crypto.md)'s.

## The key

`HashSeed` is 128 bits. It is published **once** per boot in the kernel and
**once per process** in userland, so no cross-process collision oracle exists
and a compromise of one process does not reveal another's table layout.

- The kernel publishes immediately after the CSPRNG output reserve is seeded,
  from the same reserve and at the same point as the per-boot identifier, and
  audits the result (`HashKeyPublished` / `HashKeyUnavailable`).
- A userland program publishes through `tairix_rt::hash_seed()`, which draws
  from a non-blocking `random_get` the first time a caller wants the key.
  Drawing at `_start` instead would put a syscall in *every* program's entry
  path to serve the few that hash attacker-chosen input — and every EL0 test
  fixture's syscall allow-list would have to carry it. The key is still
  unpredictable and still published exactly once; only the moment it is drawn
  differs. The draw is non-blocking, so a program never parks for entropy.

The crate never draws the key itself: it is injected. That keeps it free of
external dependencies and host-testable, and leaves the boot path to decide
where entropy comes from.

`published()` reports whether a key exists yet. A consumer whose hash is over
untrusted keys refuses to run unkeyed rather than silently using a predictable
key; a consumer whose hash is not a security decision and must work before the
CSPRNG is up names `HashSeed::UNKEYED` explicitly, so the choice cannot be made
by accident. A platform whose entropy source cannot seed the reserve — where
`random_get` itself fails closed — is the only case that arises.

`HashSeed`'s `Debug` renders `HashSeed(<redacted>)`, and `SipHash13`'s does the
same for its state (whose initial value is the key combined with public
constants), so a key cannot reach a log through a derived `Debug` on an
enclosing type.

## Building a container's hasher

A container stores a `BuildHasher` rather than a hasher, and asks it for a
fresh one per key. The two shims are where the security decision is written
down at the use site:

| Shim | Construction | For |
|---|---|---|
| `BuildSipHash13` | `keyed()` — refuses with `Unseeded` before a key is published | keys an attacker can choose or influence |
| `BuildSipHash13` | `with_seed(k)`, `UNKEYED` | a holder with its own key; a hash that is not a security decision |
| `BuildFastHash` | `new()`, `with_seed(n)` | keys the kernel assigns itself |

`BuildSipHash13` has no `Default`, so a container cannot end up keyed by
accident: [`tairix-collections`](collections.md)'s `HashMap` takes its hasher
explicitly and the choice is visible wherever a map is created. Its `Debug`
redacts the key like `HashSeed`'s does.

## Consumers

| Site | Hash | Why |
|---|---|---|
| `kernel/core`'s futex bucket index | `SipHash13` | The address half of a wait key is the caller's to choose, and the bucket table is machine-wide: a predictable index lets one process crowd every unrelated futex onto one bucket lock. |
| `lib/net`'s bond transmit flow hash | `SipHash13` | A remote peer chooses the 4-tuple; a predictable hash lands every flow of an attack on one member link. |
| `lib/pagezero`'s self-verify fingerprint | `FastHash` | The buffer is the crate's own scratch. |
| `lib/net`'s multicast revision counters | `FastHash` | Folded over this host's own configured addresses. |
| `kernel/tairix-kernel`'s build-provenance id | `FastHash` | Distinguishes developer working trees; the image's integrity guarantee is the reproducible build and signed SBOM. |
| `kernel/mem`'s DMA-window allocation index | `FastHash` | The keys are that allocator's own page-aligned window addresses, and the window is private to one process, so a caller steering its own allocations can only lengthen its own probes. |

## Determinism across ports

Integer writes are little-endian and pointer-sized values are widened to 64
bits, so a value hashes identically on `x86_64`, `aarch64`, `riscv64`, and
`wasm32`.

## Testing

Both implementations are pinned by published vectors rather than by
themselves:

- `SipHash13` against the published SipHash reference vector set — the
  incrementing byte prefixes `[]`, `[0x00]`, … `[0x00 … 0x3e]` under the
  reference key `00 01 … 0f` — cross-checked against two independent
  implementations before it was written down.
- `FastHash` against the XXH64 reference implementation's published outputs.

Beyond the vectors: streaming-versus-one-shot agreement over every chunking,
endianness and pointer-width independence, bucket-spread and avalanche
counters, and the one-shot publication semantics. `cargo xtask fuzz` drives the
`fuzz_hash` harness over arbitrary bytes, lengths, and write splits.

## Stability

**experimental.** The public API may change until the first tagged release.
