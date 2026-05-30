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

## The Arch HAL (`kernel/arch/api`)

The §17.2 architecture surface lives in its own crate, `kernel/arch/api`
(`rustos-arch-api`). It is `no_std` and dependency-free, so the kernel
can name the HAL without inheriting an architecture, and a port can
implement the HAL without naming a concrete kernel crate — the two sides
meet only here. Today the crate carries the scheduler-facing slice of
the surface (`CpuId`, `SchedulerArch`); the remaining HAL primitives
(context switch, MMU/TLB, timer, interrupt entry/exit, per-CPU storage,
boot discovery) migrate here as the §17 burn-down advances.

`kernel/arch/x86_64` implements `rustos_arch_api::SchedulerArch` for
`X86_64Arch` and no longer names `kernel/sched`; `kernel/sched`
re-exports the HAL trait so existing `rustos_kernel_sched::SchedulerArch`
paths resolve to the single canonical definition. The riscv64 port still
reaches concrete kernel crates through its boot pipeline and remains a
tracked grandfathered defect.

## `cargo xtask cfg-check`

Scans every workspace `.rs` file and fails if a `cfg` predicate names
`target_arch` or `target_pointer_width` outside the allow-list of §17.2:
the architecture ports under `kernel/arch/<target>/` and the build glue
(`.cargo/`, `tools/mkimage/`, `tools/xtask/`). Target-conditional code
anywhere else means the Arch HAL boundary has leaked. As with
`deps-check`, the directories that violate the rule today are listed in a
shrink-only grandfather set.

### Freestanding integration-test harness

The freestanding QEMU integration binaries under `tests/integration/`
compile two ways: as bare-metal `no_std`/`no_main` kernels for a QEMU
target, and as inert host stubs for `cargo build --workspace`. Choosing
between those forms is a target decision, so it cannot live in the test
source — that would name the instruction set outside the architecture
ports.

Instead it lives in one audited build-glue crate,
`tests/integration/harness` (`rustos-itest-harness`). Each test crate
calls `rustos_itest_harness::emit_target_cfg()` from its build script;
the helper inspects the cargo target and enables custom cfgs:

- `freestanding` — a bare-metal (`os = "none"`) target; compile the
  kernel body.
- `itest_x86_64` / `itest_riscv64` — the freestanding x86_64 / riscv64
  ports.

Every binary and the shared `virtio_qemu_support` library gate on those
names (`#[cfg(itest_x86_64)]`, `#[cfg(not(itest_x86_64))]`, …) rather
than on `cfg(target_arch …, target_os = "none")`, so `cfg-check` scans
the tree with no grandfather entry for it.

## Headless builds

`cargo xtask build --headless` excludes every `userland/gui/*` crate from
the image, exercising the first-class headless configuration required by
§17.3. The headless image must build for every Tier-1 target.
