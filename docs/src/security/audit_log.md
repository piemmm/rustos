# Audit-log integrity

The append-only security log under `/System/Logs` is **tamper-evident**
(`AGENTS.md` §19.4): an attacker who gains write access to the log store
must not be able to alter, reorder, or delete an existing entry without
the change being detectable. This page documents the cryptographic
backbone that delivers that property — the `chain` module of
[`rustos-log`](../lib/log.md).

## Threat

Logs are the record used to *detect* and *reconstruct* a compromise. An
attacker whose first act is to scrub the log of their intrusion defeats
both. Defending the log therefore cannot rely only on access control on
the log file; the log's *contents* must carry their own integrity proof
so that any post-hoc edit is visible even to a reader who only sees the
final bytes.

## Hash chain

Each CPU owns one `LogChain`. Appending a record produces a
`ChainedEntry` whose `entry_hash` is

```
SHA-256( prev_hash(32) || seq(8, LE) || cpu(4, LE) || payload_digest(32) )
```

where:

* `prev_hash` is the `entry_hash` of the previous entry on that CPU's
  chain, or the all-zero `GENESIS_ANCHOR` for the first entry;
* `seq` is a strictly monotonic, contiguous per-CPU sequence number
  starting at `0`;
* `cpu` is the issuing CPU id (a chain is per-CPU, matching the §19.4
  "monotonic per-CPU sequence number" requirement);
* `payload_digest` is the SHA-256 digest of the serialized record bytes,
  computed by the caller.

Because each `entry_hash` feeds into the next entry's `prev_hash`,
editing or removing any entry changes every later hash. The field order
and little-endian encoding are part of the on-disk audit-log contract
and may not change without an audit-log format version bump.

The append path hashes a single fixed-size stack buffer and performs no
allocation, so it is safe to call from the kernel logging hot path.

## Verification

`verify_chain` (and the genesis-anchored convenience
`verify_fresh_chain`) re-derive the chain from a known starting state and
return the first inconsistency as a `ChainError`:

| Variant         | Meaning                                                        |
| --------------- | -------------------------------------------------------------- |
| `HashMismatch`  | An entry's stored hash does not match its recomputed contents. |
| `BrokenLink`    | An entry's `prev_hash` does not match its predecessor.         |
| `SequenceGap`   | Sequence numbers are not contiguous (drop or duplicate).       |
| `CpuMismatch`   | An entry belongs to a different CPU than the chain.            |

On success the verifier returns the chain root — the head hash over every
entry — which is the value a signed anchor attests to. `verify_chain`
accepts an explicit `(cpu, start_seq, start_hash)`, so a verifier can
check only the tail of a chain against a previously captured midpoint
(for example, the last signed anchor).

A discontinuity is itself a security event (§19.4): a reader that cannot
re-derive the chain treats the log as compromised.

## What is not here yet

This module is the cryptographic core only. The following §19.4
requirements build on it and are tracked in the `PLAN.md` "§19 Threat
Model and Hardening Burn-down":

* **Persistence.** Writing chained entries to `/System/Logs` depends on
  the filesystem (Stage 5).
* **Signed anchors.** Periodically signing the chain root into
  `/System/Logs/Anchors/` depends on a private-key signing API from the
  Stage 2 capability authority (`rustos-crypto` today exposes
  verification only).
* **`CAP_LOG_WRITE` partitioning.** Partitioning write authority per
  service, and gating truncation behind `CAP_LOG_ROTATE`, depends on the
  capability-checked log service.
