# tairix-memguard

Stability tier: **experimental**

The one definition of the sentinel a guarded memory region is filled with, and
of the window a hot path verifies it through.

A guard region sits below a region that must not be overrun, so a contiguous
overrun lands in the guard rather than in the neighbour. The deployment form is
an **unmapped** page — the overrun faults and nothing is written. Where no
page-table split is available the region is poisoned with a sentinel instead
and a disturbance of it is the overrun's signature; the early-boot stack is
that case by necessity, being in use before the MMU is on.

## What it provides

- `GUARD_BYTE` — the poison sentinel (`0xCC`, x86 `int3`).
- `CANARY_BYTES` — the width of the O(1) window immediately below the guarded
  region, which a downward overrun crosses first.
- `canary_window` — that window of a given guard. A guard shorter than the
  window yields the whole guard, so a short one is still checked over every
  byte it has.
- `canary_intact` — whether that window still holds the sentinel. An empty
  window answers `false`: a guard that reserved no bytes cannot vouch for the
  region above it, and fail-closed is the answer that surfaces the
  misconfiguration rather than hiding it.

## Why it is a crate

Three guards share this vocabulary and must never drift apart: the slab guard
(`kernel/mem`), the kthread stack guard (`kernel/core`), and the boot-stack
guard (`kernel/arch/<target>`). Nothing here is architecture-specific, so the
Arch HAL — a closed trait surface — is the wrong home; shared code belongs in
a library. Each guard keeps its own geometry and its own violation type: only
the sentinel and the window width are common, so a post-mortem reading any
guarded region in the kernel reads the same byte.

`no_std`, no dependencies, and no `unsafe`: the callers own the raw memory and
hand this crate a slice.
