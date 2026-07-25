# tairix-crc32c

Stability tier: **experimental**

The one first-party definition of TAIRiX's fast, **non-cryptographic**
block-integrity checksum: **CRC-32C** (Castagnoli, reflected poly
`0x82F6_3B78`, init/xorout `0xFFFF_FFFF`).

It catches media / transport corruption (bit rot, torn writes, misdirected
reads) cheaply, and is verified *before* the expensive cryptographic checks on
a read. It is not a security primitive — authenticity rests on the AEAD tag and
the cryptographic content hash — so a first-party implementation is permitted.

## What it provides

- `crc32c_portable` — a portable, always-correct table-driven baseline
  (the table is generated at compile time from the polynomial).
- Per-architecture hardware candidates, compiled behind a build-script-emitted
  `crc32c_<arch>` cfg (the `lib/abi-trap` precedent, so no `cfg(target_arch)`
  in the guarded source):
  - x86_64: SSE4.2 `crc32` (`CpuFeature::Sse42`).
  - aarch64: ARMv8 `crc32c*` (`CpuFeature::Crc32`).
  Both are general-purpose-register instructions — no vector state, no
  freestanding-SIMD codegen risk.
- `resolve` / `resolve_pinned` — select the implementation **once** from a
  delivered `CpuFeatureSet`, through the generic `lib/cpuops` framework:
  capability-gated, self-verified bit-for-bit against the portable reference,
  and fail-closed to the baseline. Returns a typed `Decision` for the audit
  log.
- `checksum` — the hot-path entry; reads the set-once resolved function pointer
  (no code patching, W^X-clean) and falls closed to the portable baseline
  before `resolve` runs.

## Selection is capability-only

The hardware path is chosen by `Selection::ByPriority` — hardware CRC-32C is
unconditionally faster than the table baseline and bit-identical, so there is
nothing to benchmark. An absent instruction is never reached: the candidate is
filtered out unless its feature bit is present, and a decode bug is caught by
the mandatory self-verify before the candidate can be selected.

See `plans/FIX-HARDWARE-FEATURES.md` (P2) and
`docs/src/architecture/cpu-feature-dispatch.md`.
