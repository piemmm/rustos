# `rustos-log`

Structured, level-filtered, allocation-free logging.

## Model

* `Event` — a borrowed record carrying a `Level`, a stable `EventId`, a
  short message, and an optional slice of `Field`s. A `Field` is a `&str`
  key plus a typed `FieldValue` — the one field-value model (below), so a
  caller logs a real integer, error code, capability id, address, or bounded
  string rather than a pre-formatted string. Console sinks render a value
  through its `Display` impl.
* `Sink` — trait implemented by anything that can receive an `Event`
  (a serial port, a kernel ring buffer, an IPC pipe). Sinks must not
  panic and must not allocate on the hot path.
* `log(sink, event)` — single dispatch entry point; performs one
  relaxed-atomic load and one comparison before deciding whether to
  forward the record.

A single global `AtomicU8` holds the current `max_level`; values below
the threshold are dropped before reaching the sink, so a `Trace` call in
production code costs the same as an inlined branch.

## Event identifiers

`EventId` is a `u32` newtype. The numeric values are part of the contract
with external log consumers (operator dashboards, audit pipelines): once
published, an ID is fixed forever. New events take the next free integer
inside their subsystem's reserved 1 000-wide range — for example
`1_000..2_000` for `kernel/sec`, `2_000..3_000` for `kernel/mem`.

## Tamper-evident audit chain (§19.4)

The `chain` module provides the cryptographic backbone for the
append-only security log under `/System/Logs` (`AGENTS.md` §19.4). See
[Audit-log integrity](../security/audit_log.md) for the full model.

* `LogChain` — one per stream; `append(cpu, payload_digest)` issues a
  `ChainedEntry` binding the previous entry's hash, a monotonic append
  sequence number, the record's originating CPU id (bound as evidence),
  and the caller-supplied payload digest. The append path hashes a single
  fixed-size stack buffer and never allocates.
* `ChainedEntry` — a self-describing record; `recompute_hash` /
  `is_self_consistent` re-derive its hash so a verifier never trusts a
  stored hash it did not recompute.
* `verify_chain` / `verify_fresh_chain` — walk a slice of entries,
  reporting the first `ChainError` (`HashMismatch`, `BrokenLink`,
  `SequenceGap`) and otherwise returning the chain root. A tampered `cpu`
  is caught as `HashMismatch`, since it is bound into each entry's hash.

The module is payload-format agnostic: the persisted-log writer reduces
each serialized record to a `rustos-crypto` SHA-256 digest, so this
crate pulls in no payload codec and stays allocation-free.

## Streams and on-disk segments

* `Stream` — the closed set of log streams (`boot`, `runtime`, `debug`,
  `security`, `audit`, `journal`). The discriminant is a stable on-disk
  value; `genesis_label` feeds `stream_genesis`, and audit/security
  streams `requires_seal`.
* `segment` — the append-only on-disk container for one stream: a
  self-checksummed `SegmentHeader`, length-framed records each carrying
  their `LogChain` link hash, and a `SegmentFooter` with the record /
  sequence / time bounds, the segment hash, an optional seal MAC
  (mandatory for audit/security), and a footer checksum. `SegmentWriter`
  builds one into a caller buffer; `SegmentReader` is a forward-scanning,
  self-verifying reader that recovers to the last complete chain-valid
  record after a torn write; `verify_segment` fully checks a closed
  segment and returns a `SegmentSummary`. The container is
  payload-agnostic (record bytes are opaque) and every hash is over a
  contiguous byte range, so no streaming hash is needed.

## Logical record model (`record`)

A committed log entry has two layers. The **physical container** (`segment`,
above) owns the record's stream, append sequence, originating CPU, monotonic
time, boot id, and the whole integrity group (per-record and per-segment
hashes, the optional seal). The `record` module owns the **logical record
body** the container carries as an opaque payload — everything else in the
`plans/SYSLOG.md` §5 model, encoded once and never duplicating a
container-owned field.

* `LogRecord` — the borrowed encode input: the authoritative
  `effective_level` (`Level`), the per-CPU `cpu_seq`, the per-record
  `WallClockReading` (wall time + trust state), the kernel-attested `Origin`,
  the system-derived `source_name`, the `CallerContent`, and the flat set of
  `data.*` `(FieldName, FieldValue)` pairs. `encode` writes a compact
  little-endian body, checking every bound and rejecting a violating record
  whole.
* `CallerContent` — the caller-supplied portion the journal stores faithfully
  but never treats as authority: the optional caller `level`, `component`,
  `tag`, `event_id`, `requested_source`, and `requested_stream`, plus the
  required `message`.
* `decode` / `LogRecordRef` — a fail-closed decoder returning a validated
  borrowed view. Every length, discriminant, and UTF-8 constraint is checked,
  each `data.*` key is re-validated against the `FieldName` grammar, and the
  fields must tile the body exactly (no trailing bytes). `DataFieldIter` walks
  the validated `data.*` pairs.

The body reuses the shared building blocks — `FieldValue`/`FieldName`,
`Origin`, `WallClockReading`, `Stream`, `Level`, and the shared named-field
codec (`rustos_abi::encode_named_field` / `decode_named_field`) that the
`log_emit` diagnostic record also builds on — so there is one field-encoding
definition, not two. Origin fields the kernel cannot yet attest are absent by
construction rather than guessed.

## Authority model (`authority`)

A record has two classes of data: **system-attested** metadata the kernel or
journal vouch for, and **caller content** the emitter chose (`plans/SYSLOG.md`
§2). The `authority` module owns the two decisions that turn an attested
`Origin` plus the caller's *requests* into the authoritative values a record
carries. Both are `no_std` and allocation-free, so they run on the kernel's
early-boot path.

* `derive_source` — computes the system-derived `SourceName` from the attested
  `Origin`, never from anything the caller supplied (§3.2). A `Kernel`-domain
  origin with a valid subsystem label becomes `kernel.<subsystem>`; a
  `User`-domain origin becomes `user.<uid>.proc.<proc_id_hex>`; a kernel record
  with no (or a grammar-violating) subsystem falls back to `unknown.kernel`.
  The subsystem label is trusted kernel input but is still grammar-checked, so
  it can never smuggle a `.` and synthesise `kernel.audit.…`. `SourceName` is a
  fixed-capacity inline buffer and there is no constructor that accepts a
  caller string. The source-derivation order in the specification also names
  driver / supervised-service / signed-app classes; those need executable-role
  metadata the kernel does not yet attest (`Origin` distinguishes only
  `Kernel` from `User` today), so they are added in place here when their
  attestation producer exists rather than invented ahead of it.
* `reserved_source_prefix` / `RESERVED_SOURCE_PREFIXES` — screen a caller's
  advisory `caller.requested_source` for a reserved prefix (`kernel.`,
  `driver.`, `audit.`, `security.`, `journal.`, `service.`, `system.`). A hit
  is a spoofing attempt: it is preserved as a caller claim (evidence), never
  allowed to become the authoritative source (§3.3).
* `resolve_stream` — assigns the effective `Stream` from the caller's requested
  stream and the origin's trust (§2.3), returning a `StreamDecision`. A
  `Kernel`-domain principal is trusted for every stream, so its request is
  honoured. A `User`-domain principal may write only the caller-writable
  streams (`runtime`, `debug`); a request for a trusted-emitter stream
  (`boot` / `security` / `audit` / `journal`, per `Stream::requires_trusted_emitter`)
  is denied, downgraded to `runtime`, and `spoofed` is set so the ingress can
  preserve the request as a claim and raise a trusted security record. An
  absent request defaults to `runtime`. Finer trust (a supervised service that
  may legitimately write `security`/`audit`) grows in place when the kernel
  attests that trust domain.

## Typed-field value model (`field`)

The typed field-value model is defined in `rustos-abi` (`rustos_abi::field`,
its ABI-schema home) and re-exported as `rustos_log::field` so both the
`log_emit` record (`rustos_abi::log`) and the RustOS system log
(`plans/SYSLOG.md`) schema share **one** definition — there is no second
string-only field encoding. It defines the closed set of value types a field
may hold and how a single value is named, validated, and encoded; it does
**not** define the framed record or on-disk segment format — that is the
journal service's job.

* `FieldName` — a validated caller field name. The grammar is the
  case-sensitive ASCII identifier `[a-z][a-z0-9_]{0,63}`. Because the `.`
  separator is not in the grammar, a caller name can never collide with a
  reserved journal namespace; `reserved_prefix` screens *qualified* names
  (`record.` / `origin.` / `source.` / `integrity.` / `sys.`) at the layer
  that can contain a dot.
* `FieldValue` — the *closed* value set: null, bool, signed/unsigned 64-bit
  integer, fixed-point `Decimal`, `Time64`, `Duration64`, bounded UTF-8 string,
  bounded bytes, `Uuid`, `IpAddr`, `MacAddr`, kernel error code, capability id
  (only the public numeric id — never a raw token), and a same-type bounded
  `FieldList` of scalars. Records are flat: nested maps and nested lists are
  forbidden so search, indexing, and rendering stay cheap. The scalar ABI types
  (`Time64`, `Duration64`, `Errno`, `CapabilityId`) are reused from `rustos-abi`
  so a logged value and its ABI form cannot drift apart.
* `FieldValue::encode` / `FieldValue::decode` — a compact little-endian codec
  for a single value (a tag byte plus payload). Decoding borrows variable-length
  data from the buffer, so the model stays allocation-free. Every length, tag,
  UTF-8, and range constraint is checked and fails closed with an `Errno`;
  nothing partial is written on an encode error. `encode_list` builds a list
  value from a slice of same-type scalar elements; `FieldList` iterates the
  decoded elements lazily.
* `ToFieldValue` — the only gate between application data and the log. A
  secret-bearing wrapper type (a key, password, or capability token)
  deliberately does **not** implement it, so a secret cannot be logged by
  construction: there is no blanket impl and no `Display`/`Debug` fallback.

This model and the audit `chain` above share one SHA-256 hash chain; the log
service layers its record and segment encoders on top of these values and that
chain, never a second value model or a second chain.

## Log attestation (`attest`)

The `attest` module supplies the two cryptographic foundations the system
log's tamper-evidence rests on, without building the journal or the on-disk
segment format (those are the log service's job):

* `machine_id_hash` / `stream_genesis` — derive a log stream's hash-chain
  genesis value, binding it to *this installation* (`machine_id_hash`), the
  stream, and the kernel's per-boot `BootId` through domain-separated
  SHA-256 over `lib/crypto`. A first segment chains to this value instead of
  the all-zero `GENESIS_ANCHOR`, so a segment lifted from another
  installation, stream, or boot fails verification. The genesis is not
  secret — every input is public and a verifier recomputes it freely;
  confidentiality rests on the seal below.
* `LogAttestationKey` — a per-installation secret (a 256-bit HMAC-SHA256 key)
  that `seal`s closed audit/security segments and `verify`s them in constant
  time. It is scrubbed from memory on drop, never implements `ToFieldValue`
  or any rendering trait (so it cannot be logged), and its raw bytes never
  leave the type — callers seal and verify *through* it. `to_file_bytes` /
  `from_file_bytes` are the on-disk key-file image the image builder /
  installer provisions under `/System/Security/Keys/`, gated by the inode
  owner/mode model (system-user-owned, `0o600`) until the journal principal
  exists — no capability is minted ahead of that holder.

All sealing uses the audited HMAC-SHA256 in `lib/crypto`; this module never
names an upstream crypto crate and never hand-rolls a primitive.

## What this crate is not

It is not a re-implementation of `tracing` or `log`. The API is small on
purpose: a single sink per process is wired up at boot and never
discovered, so there is no global registry, no dynamic dispatch beyond
the trait object the caller already holds.
