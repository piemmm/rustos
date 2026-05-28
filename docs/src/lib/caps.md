# `rustos-caps`

Capability primitives: identifiers, sets, and signed delegation tokens.

## Capability sets

`CapabilitySet` is a thin wrapper around `BitSet256` keyed by
`CapabilityId`. The type-distinct wrapper exists so the rest of the system
can talk about "set of capabilities" without accidentally mixing it with
other 256-bit bitmaps.

The only delegation primitive — `CapabilitySet::delegate(requested)` —
enforces the security invariant **a delegated set is always a subset of
the parent set**. Any attempt to widen authority fails with
`Errno::DelegationWiden`. The invariant is asserted by an exhaustive
property test over all 2⁸ subsets of the well-known capabilities.

## Tokens

`CapabilityToken` is the Ed25519-signed envelope a privileged authority
issues to delegate capabilities to a task. Its on-wire layout is
documented on the type and frozen as part of `abi-v1`. Verification
checks, in order:

1. ABI version matches the kernel's.
2. Revocation epoch matches the verifier's current epoch.
3. Ed25519 signature verifies against the supplied authority key.
4. The token's set is a subset of the parent set the verifier is willing
   to grant.

This crate exposes verification only. Signing belongs to the local
capability authority service introduced in later stages and is never
linked into general code.
