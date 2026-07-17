# tairix-fuzzseed

**Stability tier: experimental** (test scaffolding; not part of any shipped
TAIRiX interface).

Shared host test-support seam for the harnesses that draw pseudo-random
inputs — the §19.6 fuzz harnesses, the §19.7 stateful proptest models, and the
filesystem soak. It is the single place (`AGENTS.md` §2.2) that:

- picks a per-run PRNG seed — **fresh entropy by default**, env-pinned for
  replay (`resolve_seed` / `start`);
- **logs the seed at the start of each test**, with the exact `VAR=value` to
  reproduce it (`announce` / `start`);
- bounds the work: a **single smoke iteration** by default
  (`smoke_iterations`), or a wall-clock soak loop (`budget_deadline` /
  `within_budget`).

It lives under `tests/` — not `lib/`, which is reserved for code that ships
inside TAIRiX — and is consumed only as a `[dev-dependencies]` entry of the
harness crates, so it never enters a TAIRiX build graph. The seed is a
*test-input* seed, not a security seed, so it deliberately does not route
through `lib/crypto` / `lib/rng` (`AGENTS.md` §22).

## Replaying a failure

A failing harness prints, before its first input:

```
[fuzzseed] <test>: PRNG seed = 12345 (0x0000000000003039); replay with TAIRIX_FUZZ_SEED=12345
```

Reproduce it in a development environment by pinning that seed and forcing the
single-iteration smoke run:

```
TAIRIX_FUZZ_SEED=12345 cargo test -p <crate> --test <harness>
# or, through the orchestrator:
cargo xtask fuzz --target <harness> --seed 12345
```
