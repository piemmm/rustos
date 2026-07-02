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
  `requires_seal` / `requires_trusted_emitter` / `is_rate_limitable`
  predicates.
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
- **`ingress`**: the admission decision (`Ingress`/`Admission`). Combines an
  attested `Origin` with the caller's stream/source/level requests: resolves
  the effective stream, derives the authoritative source, screens a
  reserved-namespace source spoof, and assigns the per-stream append `seq`
  (the one authoritative fact a caller cannot pick), then builds the
  `LogRecord` body with the caller's requests preserved as claims. It stops at
  the decision; persistence is the `journal` engine below.
- **`journal`**: the persistence engine (`Journal`/`SegmentStore`). Owns the
  `Ingress` + per-stream segment state and drives the segment lifecycle over a
  storage-agnostic sink: `commit` encodes an admitted record and appends it to
  its stream's open segment, rotating (close, seal, persist, reopen a chained
  segment) when a buffer fills; `import_boot` drains a `BootRing` into the
  `boot` stream and authors one trusted loss record for an evicted range;
  `flush` closes open segments. `admit_limited` applies the rate limit before
  reserving a sequence, and `emit_rate_loss` authors the coalesced
  `journal.rate.loss` records; `note_spoof` authors the trusted `security`
  record for a spoof. Fail-closed: an invalid/over-cap record is rejected
  whole, and an audit/security segment cannot close without the seal key.
  Per-CPU gap detection and retention remain the userland service's job on top
  of it.
- **`ratelimit`**: the per-stream ingress rate limiter (`RateLimiter`/
  `RateLimit`/`DropReport`). A `Copy`, allocation-free token bucket (integer
  nanoseconds, no float) that gates only the rate-limitable `runtime`/`debug`
  streams and coalesces drops into one trusted loss report per interval per
  stream; the system-authority streams are never dropped. Protects the machine
  from a log-driven denial of service.
- **`dict`**: the segment-local string dictionary (`DictionaryBuilder`/
  `DictionaryView`). A back-reference codec that stores a repeated provenance
  or message string once per segment and references it thereafter, with
  bounded growth (promote-on-repeat, fixed-capacity tables) and a fail-closed
  reader. No separate on-disk block: it is carried inside the records and
  covered by the record chain and segment hash.
- **`segment`**: the append-only, self-checksummed on-disk segment container
  with forward-scan power-loss recovery. Each record block carries its own
  monotonic ordering time (§5.1), covered by the segment hash.
- **`render`**: the boot-console renderer (`render_line`). Turns a decoded
  record plus its monotonic time into the canonical
  `[monotonic] level source[component]: message key=value` line. Caller text
  (message, component, requested source, string field values) is escaped so it
  can never inject a terminal escape, newline, or forged prefix; the line is
  headed by the *system-derived* source, and a downgraded `requested_source`
  spoof is shown as inert evidence. `no_std`, writes into any
  `core::fmt::Write` sink.
- **`report`**: the rich record views (`render_json` / `render_markdown` /
  `render_table_header` / `render_table_row` over a `RecordFrame` + decoded
  record) the `log` tools render for `show`/`report`/`export`. Each separates
  system-attested metadata from caller content and shows a caller's *requested*
  privileged source/stream inertly as a claim; caller text is escaped, so the
  JSON object and table row are control-byte-free and the JSON is valid. `no_std`,
  writes into any `core::fmt::Write` sink.
- **`chain`**: the per-stream SHA-256 record hash chain (`lib/crypto`), the one
  chain the log uses.
- **`bootring`**: the bounded per-CPU early-boot ring (`BootRing`). Before
  `/System/Logs` is writable each CPU retains its most recent records — the
  same logical record body plus the `cpu_seq` and monotonic time import must
  preserve — in an allocation-free FIFO over a caller-owned arena. A full ring
  evicts the oldest and accumulates a contiguous `cpu_seq` `LossRange`, so the
  journal can emit one trusted loss record naming the dropped range rather than
  leaving an undetectable gap.
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
- The advancing, bounds-checked byte cursors that `record` and `dict` share
  live once in the internal `cursor` module (one definition), distinct from
  `rustos_abi`'s offset-indexed `le` helpers for fixed-`WIRE_LEN` structs.

## Stability

Tier: `experimental`.
