# The WinterSun realm protocol

`userland/games/wintersun/net` (`tairix-wintersun-net`) is how a WinterSun
client and a WinterSun realm speak. It is the first crate of the
`userland/games/` subtree and the whole of `plans/WINTERSUN.md` WS1.

It is a **game's** protocol, not an operating system's. `lib/abi` is the
contract between userland and the kernel, and a realm's messages are no part
of it, so they live in the game's own leaf subtree — held to the same
discipline (versioned, fixed-width, little-endian, total bounded decode, fail
closed, fuzzed) without borrowing the ABI's namespace. Stability tier:
**experimental**.

## The games subtree, and the edge that keeps it a leaf

Five userland programs — the client, the three realm server binaries, and the
admin command — share one simulation, and `AGENTS.md` §17.4 forbids a
`userland/*` crate from depending on another. That is answered by giving games
their own **layer**, not by moving game code into the OS libraries.

`cargo xtask deps-check` gained `Layer::UserGame`, modelled on the existing
`Layer::UserGui`: a `userland/games/*` crate may name `lib/*` and its own
siblings, and **nothing outside the subtree may depend on it**, transitively
included. The `userland/games/` arm precedes the generic `userland/` one in
`classify`, or every game crate would fall through to plain `Userland` and its
own internal edges would be refused. The subtree is nested under `userland/`
rather than made top-level for a fail-safe reason: `classify`'s fallthrough is
`Layer::Tooling`, which is layering-*exempt*, so a forgotten arm on a
top-level tree would silently let game code name kernel internals, where a
forgotten arm under `userland/` merely over-restricts and fails loudly.

## Establishing a session

Two plaintext messages, before anything else is sent:

```text
client -> realm   Hello        version, client ephemeral key, client nonce
realm  -> client  ServerHello  realm ephemeral key, realm nonce,
                               realm identity key, signature
```

and a third — `Refused`, naming a reason — that the realm sends instead of the
second when it will not proceed, because a refusal before the keys exist still
has to say why.

Both sides then agree over X25519, hash the exact bytes they exchanged into a
transcript, and derive two directional record keys from the agreement keyed
over that transcript:

```text
dh      = X25519(own ephemeral secret, peer ephemeral key)   (contributory-checked)
master  = HMAC-SHA256(key = dh, msg = transcript)
c2s_key = HMAC-SHA256(key = master, msg = "…client-to-server records")
s2c_key = HMAC-SHA256(key = master, msg = "…server-to-client records")
```

The agreement output is a curve coordinate rather than a uniform bit string,
so it never reaches a cipher directly — it keys the PRF, and the record keys
come from that.

The realm's Ed25519 signature covers the transcript, so it authenticates *this*
exchange and cannot be lifted onto another. The ephemeral keys mean a later
compromise of the realm's identity key does not open a recorded session. Every
primitive is `lib/crypto`'s; nothing cryptographic is written in the game.

**Signing stays outside the crate.** `lib/crypto` exposes signature
verification only — private keys live behind the capability authority, not in
a wrapper any crate can link — so `handshake::respond` takes a signer callback
and the realm's identity secret never enters this code.

### Who is authenticated, and when

The handshake authenticates the **realm**, not the player. The client pins the
realm's identity key on first connect and a change is *surfaced* rather than
accepted, so a credential cannot be harvested by a substituted server.

The player authenticates afterwards, inside the encrypted session, with an
`Authenticate` message in one of three forms:

- **local** — a player on the realm's own machine, where the gateway reads the
  kernel's attestation of the connecting task and needs no secret at all;
- **password** — checked against the account's stored derivation;
- **account key** — an Ed25519 signature over `handshake::client_auth_payload`,
  which is this session's transcript, so a captured proof is worthless
  anywhere else.

A refusal carries nothing: not which of the account or the secret was wrong,
not whether the account exists. One indistinguishable failure is what stops an
attacker enumerating accounts by trying them.

## The record transport

Once the keys are agreed, every message travels as one record:

```text
[ plaintext length : u32 LE ] [ ciphertext ] [ Poly1305 tag : 16 ]
```

The nonce is the record's sequence number in its direction, and the cleartext
length header is the associated data. That one arrangement refuses a whole
family of frame attacks with no extra check:

| Interference | Why it fails |
|---|---|
| Reordered, replayed | Opened under the wrong nonce, so the tag does not verify |
| Truncated, extended | The header no longer matches the body it authenticates |
| Oversize | Refused against the fixed record bound before a body byte is read |
| Reflected | The two directions carry different keys |
| Tampered | Any bit flip fails the tag |

Any of these **ends the session**. The failure latches: every later call on
that session refuses with the same reason. A transport that skipped a bad
record and carried on would let an attacker probe it indefinitely.

An opened record's plaintext is wiped when its guard drops — unconditionally,
because the one message carrying a password is indistinguishable from the rest
until it has been decoded, and a receive buffer lives for the whole session.
The keys are wiped when the session drops.

## The message set

**Client → realm.** `Authenticate`, `SelectCharacter`, `Intent`, `Chat`,
`ConsoleCommand`, `Ping`.

A client sends **intents**, never state: "I am holding north-west", "cast
spell seven at this point". Where it is, what it hit, and what it owns are the
realm's answers, computed from intents it validated. There is no message here
that asserts a position, a hit, or a balance, and no trust level that adds one.

An intent carries a client sequence number — which the realm echoes, so the
client can replay everything after it over each authoritative state and have
its own movement feel immediate without the realm ever believing it — and the
tick and **sub-tick phase** the input was sampled at, so the realm places the
action *within* the tick rather than snapping it to the boundary. An aimed
action or cast additionally carries the view time the client is shooting at,
which the realm clamps against that peer's own measured round trip before
rewinding. All three are inputs to validate, never authority: the realm's
monotonic clock is the only clock, which closes the whole speedhack family.

**Realm → client.** `Welcome`, `AuthResult`, `Snapshot`, `Delta`,
`WorldDelta`, `Event`, `ChatMessage`, `ConsoleReply`, `Pong`, `Disconnect`.

`Welcome` carries the realm seed, its parameters, the protocol version, and
three digests: the content documents, the **world generator**, and the rules.
The generator digest is not a formality — the client generates the terrain it
walks on, so a client whose generator differs by one stage would draw ground
the realm does not simulate and diverge on collision, a defect that presents as
"I fell through the floor" and is near-impossible to diagnose from the symptom.
A mismatch on any digest is refused at connect with the reason stated, never
negotiated down.

Terrain itself is never transmitted: the world is a pure function of its seed,
so the wire carries only what the seed cannot predict — entities within
interest, and the stored deltas players caused. What the realm keeps secret — a
dungeon interior, an unopened container, an undetected trap, a player outside
your awareness — has no encoding here at all, which is a stronger guarantee
than choosing not to send it.

## Decoding

Every decoder is **total**: any byte string yields a message or a typed
`WireError`, never a panic and never a partially-applied frame.

- **Fixed width, canonically.** A variant needing fewer bytes than the widest
  one is zero-padded and the padding is *checked* on decode, so every value
  has exactly one encoding and there is no covert channel. An optional field's
  id must be zero when the field is absent, for the same reason.
- **Counts are bounded before the run they prefix is touched**, so a hostile
  count buys the decoder no work. A delta's entered and updated runs are both
  inside the interest set, so the cap bounds their *sum* — checking them
  separately would admit a frame twice the size the interest model can produce.
- **Illegal states are unrepresentable where they can be.** A heading and a
  sub-tick phase are fractions of a turn and of a tick, so no value is out of
  range. A movement direction is a Q1.15 vector checked against one unit, so a
  client cannot ask for more speed than a direction can express.
- **Text is data.** Chat, names and commands are refused if they are not UTF-8
  or carry a control character, so a message can never inject an escape
  sequence into a terminal or a console. A console *reply* admits newline and
  tab, because it legitimately wraps and tabulates, and nothing else.
- **Trailing bytes are refused**, because no encoder produces that shape.

## Bounds

`bounds` holds every fixed limit in one place. They are **security bounds on
untrusted input, not capacities** (`AGENTS.md` §24.4): they do not scale with
the machine and they do not move to admit a frame. The record bound is
*derived* from the widest message the encoders can produce and asserted at
build time, so a bound can never refuse honest traffic.

## Verification

- Host unit tests beside each module: round-trip exactness for every message
  and every variant, and a refusal at every field bound.
- Three budgeted fuzz harnesses, enrolled in `cargo xtask fuzz`:
  - `fuzz_wire` — the message decoders over mutated frames and noise, checking
    that a decoded message re-encodes to the *same bytes*;
  - `fuzz_handshake` — the handshake over hostile pre-key bytes, checking that
    a session completes only on a transcript the client itself sent;
  - `fuzz_session` — the record transport under every interference above,
    checking that each is refused and the session ends.
- A committed regression corpus (`tests/regression_corpus.rs`) with pinned
  accept/reject verdicts, so a bound that silently moves fails there.
- Oracles: the crate contains no `unsafe` and no shared mutable state — the
  codec is pure and a session is owned by one holder — so `miri` and `loom`
  have nothing here to look at. The lock-free ring the client and realm
  processes will exchange frames through is WS8's, and carries loom models
  then.
