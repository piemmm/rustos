# Modularity contracts and enforcement

`AGENTS.md` §17 guarantees that the scheduler, the architecture backend,
and the desktop can each be replaced or omitted without rewriting the
rest of the system. Those guarantees are not honour-based: two `xtask`
subcommands fail the build when the workspace drifts from them, and both
run inside `cargo xtask ci`.

## `cargo xtask deps-check`

Reconstructs the workspace dependency graph from the member manifests —
every in-workspace edge is a `path =` dependency, so no external crate is
needed to read it — and rejects three classes of defect:

- **Layering (§17.4).** Each crate is classified into a stratum from its
  directory (`lib`, the Arch HAL api/impl, the scheduler api/impl, kernel
  subsystems, `kernel/core`, drivers, userland, and the GUI). An edge is
  permitted only if the source stratum is allowed to depend on the target
  stratum. `lib/*` may depend only on `lib/*`; drivers and non-GUI
  userland may depend only on `lib/*`; only `kernel/core` may name a
  concrete architecture or scheduler crate.
- **Concrete-scheduler naming (§17.1).** A kernel crate outside
  `kernel/core` and `kernel/sched/*` may not name a concrete scheduler
  crate; the rest of the kernel depends on the policy trait instead.
- **Optional desktop (§17.3).** No non-GUI crate may reach any
  `userland/gui/*` crate, even transitively. This edge is checked with no
  exceptions: the desktop boundary is clean and must stay clean.

Only build-graph dependencies are considered; `[dev-dependencies]` are
test scaffolding and are excluded.

### Grandfathered edges

The tree predates §17 and does not yet satisfy the full layering. Every
offending edge that exists today is pinned in an explicit, commented
allow-list inside the checker. The list is append-never — it may only
shrink — and a *new* violating edge is always rejected. Each pinned edge
is a tracked defect scheduled for the §17 burn-down in `PLAN.md`.

## `cargo xtask cfg-check`

Scans every workspace `.rs` file and fails if a `cfg` predicate names
`target_arch` or `target_pointer_width` outside the allow-list of §17.2:
the architecture ports under `kernel/arch/<target>/` and the build glue
(`.cargo/`, `tools/mkimage/`, `tools/xtask/`). Target-conditional code
anywhere else means the Arch HAL boundary has leaked. As with
`deps-check`, the directories that violate the rule today are listed in a
shrink-only grandfather set.

## Headless builds

`cargo xtask build --headless` excludes every `userland/gui/*` crate from
the image, exercising the first-class headless configuration required by
§17.3. The headless image must build for every Tier-1 target.
