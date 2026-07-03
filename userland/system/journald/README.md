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
- `store` — the pure segment placement derivation the production filesystem
  sink uses (`segment_placement_for` → the `/System/Logs/<stream>/` directory
  and the `<id>.seg` file path, read from the segment's own header), plus the
  well-known identity paths (`MACHINE_ID_PATH`, `LOG_ATTESTATION_KEY_PATH`).
  Host-tested independently of any syscall.

The persistence engine (`Journal`/`SegmentStore`), the record model, the
stream/level vocabulary, and the admission authority all live in `rustos-log`;
this crate is the thin, exhaustively-testable broker over them.

## The `Run` binary

The package is also the freestanding `Run` binary (`src/run.rs`,
`rustos-journald-run`) installed at `/System/Services/journald`. It links the
pure-Rust userland runtime `rustos-rt`, and at startup:

- reads its installation identity — the non-secret machine-id
  (`/System/Security/MachineId`) and the optional log-attestation key
  (`/System/Security/Keys/LogAttestation`, which seals `audit`/`security`
  segments — without it those two streams fail closed at rotation);
- reads its own attested `Origin` (`self_origin`) and the per-boot `BootId`;
- builds a `Journal` over an FS-backed `SegmentStore` (`FsSegmentStore`) that
  writes each closed segment as its own immutable file under
  `/System/Logs/<stream>/`, deriving the placement from the segment's own
  header and `fs_sync`-ing it;
- binds `LOG_INGRESS_ENDPOINT` (unrestricted-sender; the reserved id needs
  `CAP_IPC_BIND_PRIVILEGED` to bind) and serves: receive a
  request, attest the peer origin (`call_peer_origin`), stamp the current
  monotonic + wall time, and hand it to `serve`.

Missing machine-id or boot-id fails the service closed (no logging bound to a
fabricated genesis). On the host the binary is an inert stub.

## What it does not do (yet)

Boot-ring import (draining the kernel early-boot rings into the `boot`
stream — the kernel-side ring + drain syscall do not exist yet), per-CPU
sequence-gap detection, rate-limiting/aggregation, and retention are staged
follow-on increments, as is the QEMU integration vertical that launches the
service under `init` and posts live ingress requests over IPC.

## Layering

`no_std` + `alloc`; depends only on the audited `lib/*` crates `rustos-abi`
and `rustos-log`. It never links a kernel or driver crate.
