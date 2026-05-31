# Stateful models for the capability core

The capability check is the load-bearing security decision in RustOS:
every IPC endpoint and syscall gate on it, and a single wrong answer is a
privilege escalation. `AGENTS.md` §19.7 therefore requires the
capability-critical paths — `lib/caps`, `kernel/sec`,
`kernel/ipc::dispatch`, and `kernel/syscall::dispatch` — to carry a
machine-checked specification *in addition to* their unit tests.

This page covers the **Bronze tier** (mandatory): a `proptest`-style
stateful model for each path that runs under `cargo xtask proptest` for
at least 60 s per change. The Silver (TLA+) and Gold (Verus) tiers are
tracked in `PLAN.md` and not yet scheduled.

## What a stateful model is

Where a fuzz harness (see [Fuzzing](./fuzzing.md)) hammers raw bytes
looking for a crash, a stateful model generates a *structured* sequence
of operations — a small program of commands — and replays it against the
live type and an **independent reference model**. After every command the
two are compared. When they disagree, `proptest` **shrinks** the failing
program to a minimal counterexample, which is far easier to debug than a
random byte string.

`proptest` is already a vetted, pinned dev-dependency in the workspace;
the models reuse it rather than hand-rolling a generator. It never
reaches a production build.

## The four models

Each lives in a `tests/proptest_model.rs` next to the code it checks:

* **`lib/caps`** — drives `CapabilitySet` through a random sequence of
  `insert` / `remove` / `revoke` / `delegate` / set-algebra commands
  against a `BTreeSet` reference, checking membership, cardinality,
  ascending iteration, and the delegation invariant (a delegated set is
  never a superset). A second model signs real Ed25519 `CapabilityToken`s
  and checks `verify` against an oracle of its documented error precedence
  (ABI version → epoch → signature → subset).
* **`kernel/sec`** — drives a `CapTable` of `TaskCapabilities` through
  `derive` / `delegate` / `revoke` / `remove` commands. It asserts that a
  derived effective set is exactly `user_grant ∩ manifest_request` (no
  ambient authority), that delegation never widens and a refused
  delegation is a no-op, that revoke only shrinks, and that the registry's
  contents match the model.
* **`kernel/ipc`** — drives a capability-checked `Port` through
  `send` / `recv` / `destroy` commands against a reference mailbox,
  checking the fail-closed send precedence (closed → capabilities → size
  → capacity), FIFO byte-for-byte delivery, and the capacity bound.
* **`kernel/syscall`** — drives `Dispatcher::dispatch` with random caller
  capability sets and syscall selectors (known, in-range-unassigned, and
  out-of-range numbers). With always-valid arguments, the only rejection a
  known caller can provoke is the capability gate, so the model pins the
  §5.4 decision and checks that a handler is reached exactly on the calls
  the oracle accepts.

## Two run modes

A plain `cargo test` runs each model as a fast, fixed-case sweep so the
normal suite stays quick. The orchestrator turns the same models into
wall-clock runs:

```text
cargo xtask proptest                # --quick: ≥ 60 s per model (the CI budget)
cargo xtask proptest --soak         # ≥ 24 h per model (nightly)
cargo xtask proptest --list         # list the registered models
cargo xtask proptest --target sec   # run one model
cargo xtask proptest --secs 5       # custom budget (local iteration / tests)
```

The orchestrator exports `RUSTOS_PROPTEST_BUDGET_SECS`. A model reads it
and, when it is positive, keeps running fresh batches from its
deterministic proptest RNG until the budget elapses; when unset it runs
the fixed sweep. The RNG is seeded deterministically, so a counterexample
is reproducible regardless of how far a given machine got.

## CI integration

`cargo xtask proptest --quick` is part of `cargo xtask ci`, running right
after the fuzz gate. It **fails closed**: a counterexample, hang, or
invariant failure in any model fails the pipeline. The 24-hour `--soak`
mode is run by the nightly job outside `ci`.

## Draft discipline

§19.7 lets AI assistance *draft* specifications, models, and harnesses,
but the verifier is the only oracle: a draft must be reviewed by a human
under the §2.6 senior-developer bar before it becomes load-bearing. An
unreviewed draft carries a marker, and `cargo xtask spec-review` scans
the source tree and fails closed if any such marker reaches `main`. It,
too, runs as part of `cargo xtask ci`.
