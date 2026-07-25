# `tairix-crypto`

Stability tier: **experimental**.

The single place TAIRiX calls cryptographic code. Per the charter, no
cryptographic primitive is hand-rolled here: every function is a thin wrapper
over a vetted upstream implementation (`sha2`, `hmac`, `chacha20poly1305`,
`ed25519-dalek`), chosen so the audit footprint never exceeds a handful of
crates. The wrappers expose a deliberately *narrower* API than upstream
(fixed-size byte arrays, no upstream traits) to keep the boundary auditable.

See `docs/src/lib/crypto.md` for the full primitive list, test vectors, and the
constant-time-comparison guarantees.

## Backend availability + boot-time self-test (`backend`)

`backend` is the authoritative crypto backend-availability decision. It routes
through the generic `lib/cpuops` dispatch framework as an availability-only
(`ByPriority`, **never benchmarked**) family, driven by TAIRiX's single
authoritative CPU-feature detector rather than each upstream crate's private,
bare-metal-broken detection. Its mandatory self-verify is a **power-on
self-test**: the live SHA-256 path is checked against the FIPS 180-4 §A.1
known answers before the decision is trusted, and a failure is a fatal boot
condition in the kernel. It does not fork the crypto computation (the audited
crate owns that); it owns the availability decision, the self-test, and the
audit record. See the module rustdoc for the audited-crate boundary, including
why hardware SHA-256 on `aarch64` awaits a vetted driveable backend.
