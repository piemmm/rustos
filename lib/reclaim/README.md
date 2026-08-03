# tairix-reclaim

The single shared **reclaimable-memory model** for TAIRiX
(`plans/SMARTRAM.md`): what a reclaimable cache is, how much memory it may
hold, when it must give that memory back, and the one cache implementation
that enforces all of it.

- `model` — the classification taxonomy. A cache declares a
  `CacheCandidate` (class, owner, rebuild cost, sensitivity, invalidation
  source, reclaim rule, per-entry bookkeeping bound) and passes the
  fail-closed `classify` gate before it holds a byte; an under-declared or
  credential-bearing candidate is refused with a typed `AdmissionRefusal`.
  `CacheBudget` derives the grow/shrink watermarks from the size of the
  backing resource, never a hand-picked constant. `CacheAccounting` is the
  checked, per-class, payload/metadata-split byte ledger with saturating
  event counters. `ReclaimOwner::UserlandProcess` names a userland process
  or service that has no numeric task id to charge memory against (`abi-v1`
  gives a process no way to read its own id back) — used by a library
  embedded in many different consumer programs, or by a singleton system
  service, each naming itself directly rather than mislabelling its memory
  as belonging to a desktop session.
- `pressure` — the five-band pressure vocabulary (`normal`, `mild`,
  `moderate`, `severe`, `critical`) shared with `plans/SWAPSWAPSWAP.md`,
  the hysteresis watermarks derived from the backing size, the reserve
  floor, and `shrink_target`: the deterministic map from a band and a
  class to the byte ceiling that class must shrink to.
- `cache` — `ReclaimCache<K, V, E>`: the one bounded, generation-
  invalidated, pressure-governed, wiping, self-poisoning LRU cache. A hit
  is O(log n); a forced shrink visits only the entries it releases; a
  cache that cannot admit a value still hands the caller a usable one
  (`Served::Uncached`), so caching is never required for correctness.
- `ledger` — `CacheLedger`: one cache described for a diagnostics
  registry (its label, owner, and class plus a shared handle to the
  counters above) and the one conversion of that description into the
  System Information wire record. The kernel's statistics registry and the
  userland runtime's cache reporter both publish that record and neither
  may depend on the other, so the conversion lives beside the model that
  defines a class and an owner — a kernel row and a reported row cannot be
  spelled differently. A sampled record deliberately leaves its origin
  unset: whoever publishes it stamps that from an attested identity, so a
  process cannot present its own figures as measured ones.
- `audit` — the stable `2000` / `2001` audit events a classification
  refusal or a detected ledger defect emits through `lib/log`.

## Two gauges, one band

`PressureGauge` has exactly two implementations because there are exactly
two vantage points:

- `MemoryPressure` **measures**. It samples a `FreeMemorySource` — in
  production the kernel's physical frame allocator — and folds the reading
  into a band with hysteresis, so a reading hovering on one threshold
  cannot oscillate the band. It is sampled on its consumers' own
  operations; there is no background worker and no tick.
- `ReportedPressure` **receives**. A userland process cannot see free
  frames, watermarks, or the reserve floor, so it is told the band and
  stores it here. Until it is told, it answers `critical`: an unknown band
  admits nothing.

Both drive the same `shrink_target`, so the desktop's rasterised-asset
caches give memory back in the same order, at the same bands, as the
kernel's own caches.

The figures travel the other way for the same reason. A process's heap is
its own, so nothing outside it can measure what its glyph atlas or its
decoded artwork is holding; left there, the `disposable-ui` class total
would read zero on a desktop holding megabytes of exactly that. Every
cache therefore hands out a `CacheLedger`, the kernel registers its own
and the runtime reports the process's, and the two sets are folded into
one set of class totals by the System Information service. Reported
figures stay clearly marked and stay in user space: the kernel's own
reclaim decisions read only the ledgers the kernel measures.

## Why it lives in `lib/`

Memory pressure is a property of the machine, not of privilege level. The
kernel memory manager and the desktop session both need the same
classification taxonomy, the same budgets, the same band vocabulary, and
the same eviction ordering; a second copy on either side of the syscall
boundary would let them drift, which is exactly the duplication
`AGENTS.md` §2.2 forbids. `userland/*` may not depend on `kernel/*`
(§17.4), so the one definition sits here and both sides import it.

The parts that genuinely belong to one side stay with that side: the
`ramzip` handoff and the VM escalation ladder need the kernel's own
anonymous-memory tier and live in `kernel/mem::pressure`, which also binds
the physical frame allocator to `FreeMemorySource`.

## Stability tier

`experimental` — the SMARTRAM classification, budget, and pressure seam. It
is `no_std`, allocates only through `alloc` for its own indices, contains no
`unsafe`, and every entry point fails closed rather than guessing.
