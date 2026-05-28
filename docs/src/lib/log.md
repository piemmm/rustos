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

## What this crate is not

It is not a re-implementation of `tracing` or `log`. The API is small on
purpose: a single sink per process is wired up at boot and never
discovered, so there is no global registry, no dynamic dispatch beyond
the trait object the caller already holds.
