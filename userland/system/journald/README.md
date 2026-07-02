# rustos-journald — journal service dispatch core

Stability tier: **experimental**

`rustos-journald` is the architecture-neutral **dispatch core** of the RustOS
journal service. It is the policy layer that turns one framed
`rustos_abi::log_ingress::LogIngressRequest`, plus the caller's
kernel-attested `Origin`, into an admitted, committed system-log record.

## What it does

- `service::serve` — decode and fully validate an ingress request, resolve the
  caller's *advisory* stream and level against the closed `Stream`/`Level`
  vocabularies (fail-closed on an unknown discriminant), build the `data.*`
  field set (rejecting any name outside the strict `FieldName` grammar, which
  structurally forbids reserved prefixes), admit the record under the attested
  origin — never a caller claim — commit it to the injected
  `rustos_log::Journal`, and author a trusted `security` record for any spoof
  attempt (a privileged-stream or reserved-source request), preserving the
  exact claim. Every path fails closed.
- `store` — the pure `/System/Logs/<stream>/<id>.seg` segment-path derivation
  the production filesystem sink uses (host-tested independently of any
  syscall).

The persistence engine (`Journal`/`SegmentStore`), the record model, the
stream/level vocabulary, and the admission authority all live in `rustos-log`;
this crate is the thin, exhaustively-testable broker over them.

## What it does not do (yet)

The service **binary** — which binds the well-known
`rustos_abi::log_ingress::LOG_INGRESS_ENDPOINT`, reads each caller's peer
origin, loads the installation's log-attestation key and identity material,
and drives this core over the real filesystem — is a staged follow-on, along
with boot-ring import, retention, and rate-limiting/aggregation.

## Layering

`no_std` + `alloc`; depends only on the audited `lib/*` crates `rustos-abi`
and `rustos-log`. It never links a kernel or driver crate.
