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

## What this crate is not

It is not a re-implementation of `tracing` or `log`. The API is small on
purpose: a single sink per process is wired up at boot and never
discovered, so there is no global registry, no dynamic dispatch beyond
the trait object the caller already holds.
