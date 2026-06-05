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

Offending edges that predate §17 are pinned in an explicit, commented
allow-list inside the checker. The list is append-never — it may only
shrink — and a *new* violating edge is always rejected. Each pinned edge
is a tracked defect scheduled for the §17 burn-down in `PLAN.md`.

That list is now empty: the layering is satisfied with no exceptions. The
final edges were the x86_64 production binary's bring-up dependencies
(`rustos-kernel → kernel/core`, the arch port, the driver host, and the
boot-time bus driver). `rustos-kernel` is the image-assembly seam, not a
kernel subsystem, so it is classified as `Tooling` (outside the product
layering) rather than grandfathered — the x86_64 analogue of the
downstream `tests/integration/riscv64_boot` consumer, which wires the
riscv64 image together the same way.

## The Arch HAL (`kernel/arch/api`)

The §17.2 architecture surface lives in its own crate, `kernel/arch/api`
(`rustos-arch-api`). It is `no_std` and names a single `lib/*`
dependency — `rustos-abi`, itself `no_std`, dependency-free, and
allocator-free — so the kernel can name the HAL without inheriting an
architecture, and a port can implement the HAL without naming a concrete
kernel crate — the two sides meet only here. Today the crate carries the
scheduler-facing slice (`CpuId`, `SchedulerArch`), the §19.1
side-channel slice, the §19.10 memory-tagging slice, the user-entry
slice, and the early-boot platform-discovery slice (`PlatformDiscovery`,
below); the remaining HAL primitives (context switch, MMU/TLB, timer,
interrupt entry/exit, per-CPU storage) migrate here as the §17 burn-down
advances.

`kernel/arch/x86_64` implements `rustos_arch_api::SchedulerArch` for
`X86_64Arch` and no longer names a scheduler crate; `kernel/sched/api`
re-exports the HAL trait so existing `rustos_kernel_sched_api::SchedulerArch`
paths resolve to the single canonical definition. `kernel/arch/riscv64`
is likewise a pure HAL implementation: `RiscvArch` implements
`SchedulerArch`, and the boot orchestration that used to name concrete
kernel crates (the `BootInfo` assembly, the `KernelArch` wrapper, the
boot-state slots, and the `IrqController` bridge over its PLIC) moved
into the downstream Tooling crate `tests/integration/riscv64_boot` —
the riscv64 analogue of how x86_64 keeps that pipeline in `rustos-kernel`.
Both ports now name only `kernel/arch/api` + `lib/*`.

### Arch HAL conformance vertical

Parity between ports is *enforced*, not asserted by inspection
(`plans/WIRING.md` Stage W0). `kernel/arch/api` carries a
`conformance` module — the architecture analogue of the
`kernel/sched/api` policy conformance suite — written purely against the
HAL traits so it names no concrete port:

- `conformance::run_scheduler_arch(arch)` checks the `SchedulerArch`
  contract: `current_cpu` stable across back-to-back calls, `ticks_now`
  monotonically non-decreasing, `send_ipi` to self (and to a stray
  target) a panic-free no-op equivalent, and `core_class` total — it
  returns a stable, valid class for every `CpuId`, including an
  out-of-range one.
- `conformance::run_all(arch, side_channel, memory_tagging, discovery)`
  runs that suite **and** the §19.1 side-channel vertical
  (`sidechannel::conformance`), the §19.10 memory-tagging vertical
  (`memtag::conformance`), and the §18 platform-discovery vertical
  (`platform::conformance`) over the same port's handles. Each port
  implements the four traits on distinct types (`*Arch`, `SideChannel`,
  `MemoryTags`, and a discoverer), so the suite takes one reference per
  trait.

### Early-boot platform discovery

The `PlatformDiscovery` slice normalises each target's native hardware
source into the single `lib/abi` hardware tree (`hwtree`, §18.1/§18.2):
x86_64 reads the ACPI MADT (`AcpiDiscovery`), riscv64 and aarch64 read a
flattened device tree (`FdtDiscovery`, on the shared `lib/fdt` parser),
and wasm32 queries the JavaScript host (`HostCapabilityDiscovery`). A
discoverer pushes root-first `HwNode`s into a caller-owned `HwNodeSink`,
so the trait chooses no allocator (the kernel collects on the stack, the
device manager into a growable buffer). `platform::conformance::run`
asserts the contract — at least one node, exactly one root, unique ids,
every non-root parent emitted before its child, every device class
decodable, and a full sink surfaced rather than silently dropped.

The api crate's own `tests/conformance.rs` drives the harness over an
in-test double (it cannot name a concrete port without inverting the
§17.4 layering). The real per-port coverage lives in each
`kernel/arch/<target>` crate's host test
`kernel_arch::tests::passes_arch_hal_conformance_suite`, which
instantiates `conformance::run_all` over its real handles. All four
Tier-1 ports pass.

## `cargo xtask cfg-check`

Scans every workspace `.rs` file and fails if a `cfg` predicate names
`target_arch` or `target_pointer_width` outside the allow-list of §17.2:
the architecture ports under `kernel/arch/<target>/` and the build glue
(`.cargo/`, `tools/mkimage/`, `tools/xtask/`). Target-conditional code
anywhere else means the Arch HAL boundary has leaked. As with
`deps-check`, any directory that violates the rule today is listed in a
shrink-only grandfather set; that set is currently empty — no workspace
source names the target instruction set outside the allow-list.

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

### Freestanding production kernel binary

The `rustos-kernel` crate has the same two-form shape: a bare-metal
`no_std`/`no_main` kernel for `x86_64-unknown-none` and an inert host
stub for `cargo build --workspace` / `cargo test`. Choosing between them
is a target decision, so it lives in the crate's build glue rather than
in the source. `kernel/rustos-kernel/build.rs` derives the bare-metal
condition from `CARGO_CFG_TARGET_OS`/`CARGO_CFG_TARGET_ARCH` and emits a
single custom `freestanding` cfg (declared with `rustc-check-cfg`). The
crate gates its `#![no_std]`/`#![no_main]` attributes, the
`boot`/`panic_ctx`/`serial_sink` modules, the IO-APIC typed publication
slot, and the fail-closed `halt` on `#[cfg(freestanding)]` rather than
`cfg(all(target_arch = "x86_64", target_os = "none"))`, so the target
choice stays in the one audited build-glue file.

## Headless builds

`cargo xtask build --headless` excludes every `userland/gui/*` crate from
the image, exercising the first-class headless configuration required by
§17.3. The headless image must build for every Tier-1 target.
