# tairix-caps

The capability primitives every component uses to talk about authority:
`CapabilitySet`, an allocation-free set of `tairix_abi::CapabilityId`s, and
`CapabilityToken`, the Ed25519-signed envelope that delegates a set to one
task.

## Design

- A delegated set is always a subset of its parent. `CapabilitySet::delegate`
  enforces it, and `CapabilityToken::verify` checks it again, so a correctly
  signed token naming a wider set is still refused.
- A token is bound to one subject task and one revocation epoch, both inside
  the signed body: presented by another task, or once the authority's epoch
  has moved on, it is refused.
- `no_std`, no allocation, no panics; every failure is a `tairix_abi::Errno`.
  Production code only verifies, through `lib/crypto`; signing is test-only.
- The capability-critical paths run under a stateful model
  (`tests/proptest_model.rs`, `cargo xtask proptest`).

## Stability

Tier: `experimental`.
