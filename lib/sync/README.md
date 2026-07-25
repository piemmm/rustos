# tairix-sync

Shared `no_std` synchronisation primitives for TAIRiX: spin / MCS / RW
locks, `SeqLock`, epoch-based reclamation, and set-once `Once` / `OnceCell`.

These primitives are foundational and free of any kernel dependency, so
they live in `lib/` where every layer may consume them (`AGENTS.md` §6,
§17.4). Kernel subsystems (`kernel/mem`, `kernel/ipc`, `kernel/irq`),
the scheduler implementation crates under `kernel/sched/`, and the
architecture ports all build on this single, deduplicated surface
(`AGENTS.md` §2.2).

See the crate-level rustdoc for the primitive catalogue and the
selection guidance.

## Stability tier

`stable` — the public surface (the lock types, their guards,
`Epoch`/`Guard`, `Once`/`OnceCell`, and the `InterruptControl` seam) is
consumed across the kernel and test trees. It is `no_std` and depends
only on `core` (and `loom` under the opt-in `--cfg loom` model-check
build).

## Features

- `lock-diagnostics` (off by default) — debug-only lock-site observation
  for the lockup watchdog (`plans/WATCHDOG.md`). When on, the spinning
  locks (`SpinLock`, which `IrqSafeSpinLock` wraps) become
  `#[track_caller]` and report their acquire/hold/release lifecycle,
  tagged with the acquiring call's source `file:line`, to an observer
  installed through the `lockwatch` module; the kernel records it per CPU
  so a wedged core's lockup report names the exact spinlock it is stuck
  on. A `tairix-kernel-core` `watchdog-diagnostics` (non-shippable
  `debug`-image) build turns it on. With the feature off the whole
  facility — the `track_caller` shim, the notes, and the `lockwatch`
  module — is compiled out, so a production lock is a bare
  compare-and-swap.
