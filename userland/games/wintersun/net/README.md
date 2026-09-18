# tairix-wintersun-net

The WinterSun realm wire protocol: the message vocabulary a client and a realm
exchange, the total bounded decode every frame is admitted through, the sealed
record transport, and the authenticated session handshake
(`plans/WINTERSUN.md` §7). Stability tier: **experimental** — the protocol
version is `1` and nothing has shipped, so a message may change shape in place
until it does.

`no_std`, no allocator, `forbid(unsafe_code)`.

## Why it is not in `lib/abi`

`lib/abi` is the contract between userland and the kernel. A game's messages
are no part of that, so they live here, in the game's own leaf subtree — held
to the same *discipline* (versioned, fixed-width, little-endian, total bounded
decode, fail closed, fuzzed) without borrowing the ABI's namespace.
`cargo xtask deps-check` enforces the subtree's one-way edge: `userland/games/*`
composes `lib/*` and itself, and nothing outside may depend on it.

## The four layers

| Module | What it owns |
|---|---|
| `handshake` | The two plaintext messages that agree a session: X25519 agreement, the realm's Ed25519 identity signing the transcript, HMAC-SHA256 over that transcript as the key schedule, and first-use identity pinning. |
| `session` | The sealed record transport: ChaCha20-Poly1305, one sequence-numbered nonce per record per direction, the cleartext length header as associated data. |
| `client` / `server` | The closed message set each direction may send, and its decode. |
| `value` | The identifiers, fixed-point geometry, entity state, world edits and play events messages are built from. |

Every primitive comes from `lib/crypto`; nothing cryptographic is written
here. Signing in particular stays outside: `lib/crypto` exposes verification
only, so `handshake::respond` takes a signer callback and the realm's identity
secret never enters this crate.

## What the record layer refuses, and why it costs no extra check

The nonce is the record's sequence in its direction and the length header is
the associated data, so one arrangement forecloses a whole family of attacks:

- **reordered or replayed** — opened under the wrong nonce, so it does not
  authenticate;
- **truncated or extended** — the header no longer matches the body it
  authenticates;
- **oversize** — refused against the fixed record bound before a body byte is
  read, so a hostile header buys no work;
- **reflected** — the two directions carry different keys.

Any of these **ends the session**. The failure latches: every later call
refuses with the same stated reason, so a peer cannot probe the transport by
sending one bad record after another. Every refusal — decode, handshake, or
record — maps to exactly one `DisconnectReason`, which is what the peer is
told before the connection closes.

## Bounds

`bounds` holds every fixed limit in one place. They are **security bounds on
untrusted input, not capacities**: they do not scale with the machine and they
do not move to admit a frame. The record bound is *derived* from the widest
message the encoders can produce and asserted at build time, so it can never
refuse honest traffic.

## What the wire will not carry

The realm is authoritative and the client is assumed hostile. There is no
message in which a client asserts where it is, what it hit, or what it owns —
only intents the realm validates. Terrain is never transmitted: the world is a
pure function of its seed and the client generates the ground it walks on.
And what the realm keeps secret — a dungeon interior, an unopened container, an
undetected trap, a player outside your awareness — has no encoding here at all,
which is a stronger guarantee than choosing not to send it.

## Tests

Host unit tests beside each module, three budgeted fuzz harnesses enrolled in
`cargo xtask fuzz` (`fuzz_wire`, `fuzz_handshake`, `fuzz_session`), and a
committed regression corpus with pinned accept/reject verdicts
(`tests/regression_corpus.rs`).
