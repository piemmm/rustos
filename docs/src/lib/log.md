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

* `LogChain` — one per CPU; `append(payload_digest)` issues a
  `ChainedEntry` binding the previous entry's hash, a monotonic per-CPU
  sequence number, the CPU id, and the caller-supplied payload digest.
  The append path hashes a single fixed-size stack buffer and never
  allocates.
* `ChainedEntry` — a self-describing record; `recompute_hash` /
  `is_self_consistent` re-derive its hash so a verifier never trusts a
  stored hash it did not recompute.
* `verify_chain` / `verify_fresh_chain` — walk a slice of entries,
  reporting the first `ChainError` (`HashMismatch`, `BrokenLink`,
  `SequenceGap`, `CpuMismatch`) and otherwise returning the chain root.

The module is payload-format agnostic: the persisted-log writer reduces
each serialized record to a `rustos-crypto` SHA-256 digest, so this
crate pulls in no payload codec and stays allocation-free.

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
