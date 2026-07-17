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
