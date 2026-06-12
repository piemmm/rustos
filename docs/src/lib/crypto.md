# `rustos-crypto`

The single place RustOS calls cryptographic code. Per `AGENTS.md` §1 no
hand-rolled primitives are allowed; this crate exposes thin wrappers
over vetted RustCrypto and dalek-cryptography implementations so the
audit footprint never exceeds a handful of crates.

## What ships in Stage 1

| Primitive | Wrapper                       | Upstream             |
|-----------|-------------------------------|----------------------|
| SHA-256   | `sha256(&[u8]) -> [u8; 32]`   | `sha2 = 0.10.9`      |
| Ed25519 verification | `Ed25519PublicKey::verify` | `ed25519-dalek = 2.1.1` |
| ChaCha20-Poly1305 AEAD | `aead::seal` / `aead::open` | `chacha20poly1305 = 0.10.1` |
| PBKDF2-HMAC-SHA256 | `pbkdf2_sha256` / `pbkdf2_sha256_verify` | `hmac = 0.12.1` + `sha2` |

Signing is **not** exposed. Signing keys live behind the local capability
authority service introduced in later stages and are never linked into
general-purpose callers. Test code in `lib/caps` exercises signing via a
dev-only dependency on `ed25519-dalek`, keeping the audited build's
attack surface to verification alone.

A first-party constant-time comparison, `ct_eq(&[u8], &[u8]) -> bool`,
also ships here (see below); it is the one sanctioned home for comparing
secret byte strings.

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

## Password derivation (§5.1)

`kdf::pbkdf2_sha256` derives a 32-byte password hash with PBKDF2-HMAC-SHA256
(RFC 8018 §5.2): a deliberately slow, salted derivation that makes offline
guessing of a stolen `/System/Security/Users` record expensive. The output
length equals the HMAC output, so exactly one PBKDF2 block is computed, and
the iteration count is a `NonZeroU32` — zero rounds is unrepresentable. It
is a standard *construction* over the same audited HMAC primitive (the same
shape as `rustos-rng`'s HMAC-DRBG), not a hand-rolled primitive
(`AGENTS.md` §2.12). `pbkdf2_sha256_verify` compares through `ct_eq`, so a
stored-hash comparison cannot leak through timing (`AGENTS.md` §19.1). The
consumer is `lib/users`, which owns the salt, the accepted cost range, and
the stored-record encoding.

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
`rustos-crypto` unit tests under the release profile (`-C opt-level=3`)
as a dedicated step.

## Test vectors

* SHA-256: FIPS 180-4 §A.1 vectors for the empty message and `"abc"`.
* Ed25519: RFC 8032 §7.1 test vector 1 (empty message); plus tampered
  signature and tampered message rejections.
* ChaCha20-Poly1305: the RFC 8439 §2.8.2 worked example, plus round-trip
  and tampered-ciphertext / tag / nonce / associated-data rejections.
* `ct_eq`: a per-position single-byte-flip sweep, a content-independent
  traversal-count check, and a fixed-seed randomised differential against
  the reference `==`.
* PBKDF2-HMAC-SHA256: the published SHA-256 re-computations of the
  RFC 6070 inputs (`("password", "salt")` at 1, 2, and 4096 iterations),
  plus input-sensitivity and tampered-hash rejections.
