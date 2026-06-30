# `rustos-log`

Structured, level-filtered, allocation-free logging.

## Model

* `Event` — a borrowed record carrying a `Level`, a stable `EventId`, a
  short message, and an optional slice of `Field` key/value pairs.
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

The `field` module is the foundational, reusable data model the RustOS system
log (`plans/SYSLOG.md`) builds its record schema on. It does **not** define the
framed record or on-disk segment format — that is the journal service's job —
only the closed set of value types a field may hold and how a single value is
named, validated, and encoded.

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

## What this crate is not

It is not a re-implementation of `tracing` or `log`. The API is small on
purpose: a single sink per process is wired up at boot and never
discovered, so there is no global registry, no dynamic dispatch beyond
the trait object the caller already holds.
