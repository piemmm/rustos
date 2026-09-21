# `tairix-crypto`

Stability tier: **experimental**.

The single place TAIRiX calls cryptographic code, and the only crate in the
workspace permitted to name a cryptographic dependency — not in a production
path, a test, or a build script. Per the charter no cryptographic primitive is
hand-rolled here: every function is a thin wrapper over a vetted upstream
implementation, and every upstream crate sits on one generation of the
RustCrypto and dalek-cryptography stacks, so the tree holds a single copy of
`digest`, `cipher`, and the curve arithmetic rather than two of each. The
wrappers expose a deliberately *narrower* API than upstream (fixed-size byte
arrays, no upstream traits) to keep the boundary auditable.

Nothing here draws randomness: every secret is supplied by the caller, and
where a construction would normally consume an RNG the deterministic form is
used instead (RFC 8032, RFC 6979, and FIPS 203's internal entry points).

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
