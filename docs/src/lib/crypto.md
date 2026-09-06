# `tairix-crypto`

The single place TAIRiX calls cryptographic code. Per `AGENTS.md` §1 no
hand-rolled primitives are allowed; this crate exposes thin wrappers
over vetted RustCrypto and dalek-cryptography implementations so the
audit footprint never exceeds a handful of crates.

## What ships in Stage 1

| Primitive | Wrapper                       | Upstream             |
|-----------|-------------------------------|----------------------|
| SHA-256   | `sha256(&[u8]) -> [u8; 32]`   | `sha2 = 0.10.9`      |
| Ed25519 verification | `Ed25519PublicKey::verify` | `ed25519-dalek = 2.1.1` |
| ChaCha20-Poly1305 AEAD | `aead::seal` / `aead::open` | `chacha20poly1305 = 0.10.1` |
| ChaCha12 keystream | `stream::chacha12_keystream` | `chacha20 = 0.9.1` |
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

* SHA-256: FIPS 180-4 §A.1 vectors for the empty message and `"abc"`.
* Ed25519: RFC 8032 §7.1 test vector 1 (empty message); plus tampered
  signature and tampered message rejections.
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
