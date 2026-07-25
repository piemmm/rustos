# tairix-pagezero

Stability tier: **experimental**

The one first-party definition of TAIRiX's fast memory-clear: the routine the
kernel uses to zero a freshly-allocated frame before it becomes user-visible
(so no stale bytes leak across a process boundary) and to scrub a frame on free
(the zero-on-free secret-hygiene guarantee).

## What it provides

- `zero_portable` — a portable, always-correct byte fill (`[u8]::fill`), the
  baseline every candidate is verified against and the implementation on any
  target without a block-zero instruction.
- Per-architecture hardware candidates, compiled behind a build-script-emitted
  `pagezero_<arch>` cfg (the `lib/crc32c` precedent, so no `cfg(target_arch)`
  in the guarded source):
  - x86_64: `rep stosb`, gated on ERMS (`CpuFeature::Erms`) for selection —
    a base-ISA instruction whose ERMS microcode path is the fastest general
    fill.
  - aarch64: `DC ZVA` cache-block zero (`CpuFeature::DcZva`), which clears a
    whole cache block without a read-for-ownership; the routine handles an
    unaligned head/tail with byte stores and clears the aligned interior with
    `DC ZVA`.
- `resolve` / `resolve_pinned` — select the implementation **once** from a
  delivered `CpuFeatureSet`, through the generic `lib/cpuops` framework:
  capability-gated, self-verified byte-for-byte against the portable reference
  (including that it zeroes *exactly* the requested region), and fail-closed to
  the baseline. Returns a typed `Decision` for the audit log.
- `zero` — the hot-path entry; reads the set-once resolved function pointer
  (no code patching, W^X-clean) and falls closed to the portable baseline
  before `resolve` runs.

## Selection is capability-only (never benchmarked)

The hardware path is chosen by `Selection::ByPriority`: a block-zero primitive
is unconditionally faster than a scalar byte loop and bit-identical in result,
so there is nothing to benchmark (Linux likewise selects `DC ZVA`/ERMS by
feature, not by timing). An absent or prohibited instruction is never reached —
the candidate is filtered out unless its feature bit is present, and a bug is
caught by the mandatory self-verify before the candidate can be selected.

## One resolved routine, kernel-wide

A kernel page-zero routine may run on any core the scheduler migrates work to,
so it is resolved once against the migration-safe *common* feature set (the
intersection over all cores) into a single function pointer — no per-call,
per-CPU table lookup on this hot path. Because the set is an intersection, any
advertised instruction is legal on every core, so a dispatched `DC ZVA` /
`rep stosb` can never trap after a migration.

See `plans/FIX-HARDWARE-FEATURES.md` (P3) and
`docs/src/architecture/cpu-feature-dispatch.md`.
