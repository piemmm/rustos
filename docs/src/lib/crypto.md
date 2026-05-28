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

Signing is **not** exposed. Signing keys live behind the local capability
authority service introduced in later stages and are never linked into
general-purpose callers. Test code in `lib/caps` exercises signing via a
dev-only dependency on `ed25519-dalek`, keeping the audited build's
attack surface to verification alone.

## Pinning

Versions are pinned exactly (`= x.y.z`). Bumping a pin is a deliberate
change that requires a fresh audit pass; the rationale must be recorded
in the commit message and in `deny.toml` if the licence or advisory
posture changes.

## Test vectors

* SHA-256: FIPS 180-4 §A.1 vectors for the empty message and `"abc"`.
* Ed25519: RFC 8032 §7.1 test vector 1 (empty message); plus tampered
  signature and tampered message rejections.
