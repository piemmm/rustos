# rustos-log

Structured, level-filtered logging plus the RustOS system-log (SYSLOG) data
model (`lib/log`, `plans/SYSLOG.md`). `no_std`, allocation-free on the hot
path, and payload-format agnostic so the same code runs in the kernel, a
freestanding driver, and a WebAssembly userland binary.

The crate is the shared logging home the specification mandates: the logical
record model, the on-disk segment container, the integrity hash chain, the
attestation/sealing primitives, and the authority model all live here. The
journal service (userland) and the kernel early-boot ring buffers build on it;
neither re-implements the record format.

## Contents

- **Diagnostic path** (`lib.rs`): the level-filtered `log`/`Sink`/`Event`
  fast path, a single atomic `Level` threshold, and stable `EventId`s.
- **`stream`**: the closed `Stream` set (`boot`/`runtime`/`debug`/`security`/
  `audit`/`journal`), its on-disk discriminants, genesis labels, and the
  `requires_seal` / `requires_trusted_emitter` predicates.
- **`authority`**: the authority model — `derive_source` (the system-derived
  `SourceName` from an attested `Origin`), `reserved_source_prefix` /
  `RESERVED_SOURCE_PREFIXES` (spoof screening of a caller's requested source),
  and `resolve_stream` (assigns the effective stream, downgrading and flagging
  an untrusted request for a privileged stream). Source derivation covers the
  domains the kernel attests today (`kernel.<subsystem>`,
  `user.<uid>.proc.<proc_id>`, `unknown.kernel`) and fails closed; richer
  classes are added in place when their attestation producer exists.
- **`record`**: the logical record body (`LogRecord`/`LogRecordRef`) — the
  fields the segment container does not already own — with a fail-closed codec.
- **`segment`**: the append-only, self-checksummed on-disk segment container
  with forward-scan power-loss recovery.
- **`chain`**: the per-stream SHA-256 record hash chain (`lib/crypto`), the one
  chain the log uses.
- **`attest`**: machine-id hashing, stream genesis, and the log-attestation key
  (HMAC-SHA256 seal/verify).

## Why its own crate

Both the userland journal service and in-kernel early-boot logging need one
identical record format, integrity chain, and authority model; keeping them
here is the single definition both reach without duplication (§2.2), while the
crate depends only on `rustos-abi` and `rustos-crypto` (the lowest layers).

## Design

- `no_std`, `#![forbid(unsafe_op_in_unsafe_fn)]`, `#![deny(missing_docs)]`.
- Allocation-free: variable-length data is borrowed on decode and derived
  values (e.g. `SourceName`) are fixed-capacity inline buffers.
- Fail-closed everywhere: every length, discriminant, UTF-8 constraint, and
  authority decision rejects or downgrades rather than guessing.
- The field-value model is re-exported from `rustos_abi::field`, its ABI-schema
  home; there is one definition.

## Stability

Tier: `experimental`.
