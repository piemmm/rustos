# `tairix-crypto`

The single place TAIRiX calls cryptographic code. Per `AGENTS.md` §1 no
hand-rolled primitives are allowed; this crate exposes thin wrappers
over vetted RustCrypto and dalek-cryptography implementations so the
audit footprint never exceeds a handful of crates.

## What ships

| Primitive | Module | Upstream |
|---|---|---|
| SHA-256 / SHA-384 / SHA-512, one-shot and streaming | `hash` | `sha2` |
| HMAC-SHA256 / HMAC-SHA512 / HMAC-SHA1 | `mac` | `hmac` + `sha2` / `sha1` |
| Poly1305 one-time authenticator | `mac` | `poly1305` |
| ChaCha20-Poly1305 AEAD | `aead` | `chacha20poly1305` |
| AES-128/256-GCM AEAD | `aead` | `aes-gcm` |
| AES-128/192/256-CTR | `cipher` | `aes` + `ctr` |
| ChaCha12 keystream, 64-bit-nonce ChaCha20 | `stream` | `chacha20` |
| Ed25519 sign, verify, key derivation from a seed | `sign` | `ed25519-dalek` |
| ECDSA and ECDH over P-256 / P-384 / P-521 | `nistp` | `p256` / `p384` / `p521` |
| X25519 key agreement | `agree` | `x25519-dalek` |
| Finite-field DH over the RFC 3526 groups | `ffdh` | `crypto-bigint` |
| ML-KEM-768 (FIPS 203) | `kem` | `ml-kem` |
| HKDF-Expand single block, PBKDF2-HMAC-SHA256, bcrypt-pbkdf | `kdf` | `hmac` / `bcrypt-pbkdf` |
| Constant-time comparison | `constant_time` | first-party |

**No other crate in the workspace names a cryptographic dependency** —
not in a production path, a test, or a build script. Signing lives here
too: build scripts that sign a fixture bundle and tests that mint a key
go through `Ed25519SecretKey`, so `ed25519-dalek` is named once.

**Nothing here draws randomness.** Every secret — a signing seed, an
agreement exponent, an ML-KEM encapsulation's `m` — is supplied by the
caller, which is the only party that knows whether it must come from the
kernel CSPRNG or from a fixture. Where a construction would normally
consume an RNG, the deterministic form is used instead: Ed25519 and ECDSA
derive their nonces from the key and message (RFC 8032, RFC 6979), and
ML-KEM's key generation and encapsulation take the caller's bytes as
FIPS 203 defines them.

**One dependency generation.** Every upstream crate sits on one generation
of the RustCrypto and dalek-cryptography stacks, so the tree holds a single
copy of `digest`, `crypto-common`, `cipher`, and the curve arithmetic rather
than two of each.

## Authenticated encryption (§4)

`aead::seal` and `aead::open` wrap ChaCha20-Poly1305 (RFC 8439). The
wrapper is **detached and in place**: callers hand in fixed-size byte
arrays for the key, nonce, and tag and a mutable message buffer (the
ciphertext overwrites the plaintext), so the wrapper needs no allocator
and stays `no_std`. It never sees the upstream `aead` traits or
`GenericArray` types, and it never generates nonces — `(key, nonce)`
reuse is catastrophic for this cipher, so nonce discipline belongs to the
caller. The one consumer today is the kernel's encrypted-swap layer
(`kernel/mem::swap`, `AGENTS.md` §4), which pairs an ephemeral per-boot
key with a monotonic counter. On any authentication failure `open`
returns the single opaque `AeadError::Authentication`, leaking nothing
about why a forgery was rejected (`AGENTS.md` §5.4).

AES-GCM comes in two forms over one implementation. `Aes128Gcm` and
`Aes256Gcm` hold a key expanded once — the AES key schedule and the GHASH
key — for a caller that seals many messages under it, such as an SSH
transport direction between rekeys; that expansion is most of what a short
message costs (a 48-byte SSH packet under AES-128-GCM falls from about 450 ns
to 80 ns). The upstream type wipes both on drop. `aes128gcm_seal` and its
siblings are the same operations for one message, keying per call.

## Stream keystream (§22)

`stream::chacha12_keystream` wraps ChaCha12 — the twelve-round reduced
variant of ChaCha20 (RFC 8439) — as a *keystream* rather than a cipher:
callers hand in a key, a nonce, and two destinations, and receive
`32 + N` contiguous keystream bytes with the first 32 in the smaller one.
Nothing here chooses a key or a nonce; as with the AEAD above that
discipline belongs to the caller, which is the only party that knows
whether its key is fresh per run.

The split destination is not a convenience: its consumer is `lib/rng`'s
fast-key-erasure generator, which replaces its key from the head of its own
keystream, and a single destination would force it to hold a scratch copy of
the whole run — a copy of unissued random output it would then have to wipe.
`N` is a const parameter, so the run is checked against the cipher's
per-nonce capacity at compile time and the wrapper needs no fallible path.

`chacha20` adds no crate to the audit surface: it is already in the tree
beneath `chacha20poly1305`, already source-pinned, and its `zeroize` feature
was already enabled — naming it directly only makes the dependency explicit.

The same module also exposes djb's original 64-bit-nonce, 64-bit-counter
ChaCha20, which `chacha20-poly1305@openssh.com` uses: that cipher takes its
Poly1305 key from block 0 and encrypts the payload from block 1 under one
nonce, which RFC 8439's 96-bit-nonce layout cannot express. The start
counter is therefore an explicit parameter, and a run that would carry the
counter past its last block is refused rather than silently restarting — and
so reusing — the keystream.

## The NIST prime curves (`nistp`)

ECDSA and ECDH over P-256, P-384, and P-521 live in one module rather than
split across `sign` and `agree`, because they share their key material and
their SEC1 point encoding; splitting them would mean two copies of that
encoding. SSH still treats them as separate algorithms and never uses one
key for both.

Each curve pairs with exactly one hash, as RFC 5656 §6.2.1 assigns them
(P-256/SHA-256, P-384/SHA-384, P-521/SHA-512). The pairing is not a
parameter, so a caller cannot weaken a curve by choosing a shorter hash.
A public key is validated when it is decoded — on the curve, not the
identity — and these curves have cofactor 1, so that is the whole of point
validation. Signature scalars are fixed-width big-endian field elements,
which is what SSH's `mpint` encoder consumes; no DER is produced or parsed.
High-`s` signatures verify, because ECDSA admits both `s` and `n - s`,
foreign implementations emit either, and SSH imposes no malleability rule.

## Finite-field Diffie-Hellman (`ffdh`)

SSH's `diffie-hellman-group{14,16,18}-*` and its group exchange (RFC 4419)
need `g^x mod p` over a safe-prime group, and no vetted pure-Rust FFDH crate
exists. The group operation is therefore composed from the constant-time
Montgomery exponentiation of `crypto-bigint`, which the NIST curve stack
already depends on: a standard construction over an audited primitive, the
same shape as PBKDF2 over the audited HMAC, rather than a hand-rolled one.

A peer value is accepted only when `1 < y < p - 1`. For a safe-prime group
whose private exponent is ephemeral and used once — every SSH key exchange —
that is the partial validation NIST SP 800-56A Rev3 sanctions, and it
excludes every small subgroup a safe prime has. Only the three RFC 3526
groups are offered: an arbitrary caller-supplied modulus would need a
primality test to validate and would make the cost unbounded (one
exponentiation over the 8192-bit group already holds a sixteen-entry
windowing table of full-width integers, about 16 KiB).

## Key encapsulation (`kem`)

ML-KEM-768 (FIPS 203) is the post-quantum half of `mlkem768x25519-sha256`,
OpenSSH's current default key exchange. It is a *hybrid*: the shared secret
is derived from this encapsulation and an X25519 agreement together, so
breaking either alone does not break the session.

Decapsulation is infallible by design. FIPS 203 §7.3 specifies implicit
rejection, so a ciphertext that was not produced for the key yields a key
derived from the seed's rejection secret rather than an error — which is
what stops a chosen-ciphertext attacker learning anything from the
distinction. A caller discovers a forgery when the session fails to
authenticate, never from the decapsulation call.

## Password derivation (§5.1)

`kdf::pbkdf2_sha256` derives a 32-byte password hash with PBKDF2-HMAC-SHA256
(RFC 8018 §5.2): a deliberately slow, salted derivation that makes offline
guessing of a stolen `/System/Security/Users` record expensive. The output
length equals the HMAC output, so exactly one PBKDF2 block is computed, and
the iteration count is a `NonZeroU32` — zero rounds is unrepresentable. It
is a standard *construction* over the same audited HMAC primitive (the same
shape as `tairix-rng`'s HMAC-DRBG), not a hand-rolled primitive
(`AGENTS.md` §2.12). `pbkdf2_sha256_verify` compares through `ct_eq`, so a
stored-hash comparison cannot leak through timing (`AGENTS.md` §19.1). The
consumer is `lib/users`, which owns the salt, the accepted cost range, and
the stored-record encoding.

`kdf::bcrypt_pbkdf` is a *foreign* format's KDF and not TAIRiX's: the
OpenSSH v1 private-key container wraps a passphrase-encrypted key with it,
so reading an existing user key requires it exactly as specified. Its
working scratch is the caller's buffer rather than one allocated here, for
the same reason the keystream writes straight into the caller's
destinations: the scratch ends the call holding derived key material, and
the holder is the only party that can wipe it.

## Backend availability and the boot-time self-test (`backend`)

`backend` is TAIRiX's authoritative crypto backend-availability decision and
its cryptographic power-on self-test (POST). It exists because a generic
per-architecture image is compiled against a conservative baseline (no
`+aes`/`+sha2` build-time floor), so any hardware acceleration a booted CPU
offers must be recovered at runtime — and crypto acceleration must be recovered
*safely*.

- **Availability only, never benchmarked.** The decision routes through the
  generic `lib/cpuops` dispatch framework as a `ByPriority` family. Crypto is
  never put on the framework's benchmark axis: choosing the "fastest" AES/SHA
  would happily select a table-driven variant that leaks keys through cache
  timing. Selection is a deterministic capability decision from TAIRiX's single
  authoritative CPU-feature detector, not each upstream crate's private
  detection (which, on a bare-metal `aarch64`, silently reports nothing because
  it depends on an operating system's `HWCAP`).
- **The self-verify is a power-on self-test.** Before the availability decision
  is trusted, the framework runs the live SHA-256 path over the FIPS 180-4 §A.1
  known-answer vectors and compares to their published digests. A crypto core
  that fails is not reported as working: the kernel emits a fatal audit record
  (`CryptoSelfTestFailed`) and halts, mirroring the FIPS discipline that a
  failed POST renders the module inoperable rather than letting the system run
  on broken cryptography.
- **It does not fork the computation.** Both the hardware and software SHA-256
  paths are the same audited `sha2` crate, which owns backend selection
  internally. TAIRiX does not transcribe the SHA-256 round function over
  intrinsics — that would be hand-rolling the primitive, which the charter
  forbids. What `backend` owns is the availability decision, the self-test, and
  the audit record.
- **Per-target reach.** On `x86_64` the audited crate selects its SHA-NI path
  from `CPUID`, which needs no operating system and is therefore correct on the
  freestanding kernel target, so the hardware-availability candidate is offered
  and recorded there. On `aarch64`/`riscv64`/`wasm32` no runtime-selected
  hardware SHA-256 path exists yet, so `backend` records the honest software
  answer. Recovering hardware SHA-256 on `aarch64` awaits a vetted, driveable
  audited backend (a supply-chain decision); it is deliberately not faked with
  a candidate that would not run.

The kernel resolves this once at boot alongside the CRC-32C family
(`kernel/core::cpuops`); the chosen backend is on the audit log via
`CpuOpsRoutineSelected`.

## Pinning

Versions are pinned exactly (`= x.y.z`). Bumping a pin is a deliberate
change that requires a fresh audit pass; the rationale must be recorded
in the commit message and in `deny.toml` if the licence or advisory
posture changes.

## Constant-time comparison (§19.1)

`constant_time::ct_eq` compares two byte slices in time that depends only
on their (public) lengths, never on their contents: every overlapping
byte pair is folded into a single difference accumulator with no
data-dependent branch and no early return. Comparing a secret — a MAC
tag, a capability-token signature, a key fingerprint — with `==` would
leak, through early-exit timing, how many leading bytes matched, which is
enough to forge the value one byte at a time. `AGENTS.md` §19.1 forbids
that, and this is the only sanctioned place to compare secret material.

The constant-time property is *tested*, not merely asserted, and without
the wall-clock timing that `AGENTS.md` §7 forbids as flaky: the
no-early-exit core is driven through an instrumented iterator that
records how many byte pairs it yields, and the tests assert that equal
inputs, a difference in the first byte, a difference in the last byte,
and an all-bytes difference all traverse exactly `len` pairs. A
short-circuiting comparison would visit only one pair on a first-byte
mismatch and fail the assertion. Because an optimiser can turn a
branchless compare into a branching one, `cargo xtask ci` re-runs the
`tairix-crypto` unit tests under the release profile (`-C opt-level=3`)
as a dedicated step.

## Test vectors

Every known answer comes from a published specification or from an
implementation outside this tree, never from the dependency under test.

* SHA-256/384/512: FIPS 180-4 §A vectors for the empty message and `"abc"`,
  plus a streaming-versus-one-shot agreement sweep across chunk boundaries.
* HMAC-SHA256/512/SHA1: known answers from CPython's `hmac` over OpenSSL,
  and — for the SHA-2 instantiations — an in-tree cross-check against the
  textbook RFC 2104 construction built from this crate's own hash wrappers,
  so a swapped digest fails twice over.
* Poly1305: the RFC 8439 §2.5.2 vector, plus block-boundary and tamper
  rejections.
* AES-CTR: NIST SP 800-38A §F.5's CTR-AES{128,192,256} vectors, plus the
  stateful-continuation property one connection depends on.
* AES-GCM: the Wycheproof project's vectors, through both the keyed and the
  one-shot forms, which are also held to agree across many messages under
  one key; plus a regression test that a rejected message leaves the buffer
  holding ciphertext and never the plaintext — the failure CVE-2023-42811
  was.
* ChaCha20 (64-bit nonce): a keystream computed from djb's original round
  function outside this tree, whose reference also reproduces the published
  all-zero-key vector, so the state layout is pinned rather than guessed.
* Ed25519: RFC 8032 §7.1 test vectors 1 and 2, signing and verifying, plus
  determinism, seed round-trip, and tampered signature/message rejections.
* ECDSA: RFC 6979 §A.2.5/§A.2.6/§A.2.7 deterministic `(r, s)` for all three
  curves. These are what prove the signing path is RFC 6979 and not a
  randomised nonce — a randomised signer could never match them.
* ECDH: the Wycheproof raw-SEC1-point vectors for all three curves.
* Finite-field DH: `g^x mod p` for all three RFC 3526 groups computed by
  CPython's arbitrary-precision `pow`, plus refusal of every degenerate peer
  value.
* ML-KEM-768: NIST's own ACVP vectors for FIPS 203 key generation and
  encapsulation, plus implicit-rejection behaviour on a forged ciphertext.
* bcrypt-pbkdf: the golden vectors from the Go project's independent
  implementation of the same OpenBSD construction.
* ChaCha20-Poly1305: the RFC 8439 §2.8.2 worked example, plus round-trip
  and tampered-ciphertext / tag / nonce / associated-data rejections.
* ChaCha12 keystream: the first 96 bytes under RFC 8439's test-vector key
  and nonce, computed independently from the round function reduced to
  twelve rounds rather than restated from the dependency — so the test pins
  both the round count and the split point. Plus destination-overwrite (not
  XOR-into) behaviour, run extension, and key/nonce sensitivity.
* `ct_eq`: a per-position single-byte-flip sweep, a content-independent
  traversal-count check, and a fixed-seed randomised differential against
  the reference `==`.
* PBKDF2-HMAC-SHA256: the published SHA-256 re-computations of the
  RFC 6070 inputs (`("password", "salt")` at 1, 2, and 4096 iterations),
  plus input-sensitivity and tampered-hash rejections.
