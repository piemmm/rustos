# Kernel entry, init order, and panic policy

This page documents the architecture-neutral kernel core (`kernel/core`),
delivered by Stage 2.6 of [`PLAN.md`](../../../PLAN.md). The crate ships
**three** things and nothing else:

1. The hand-off type `BootInfo` and the `KernelArch` trait an
   architecture port (Stage 3) implements.
2. The single public entry point `kernel_main` that orchestrates
   subsystem init in a fixed, documented order.
3. The panic helper `handle_panic` that an arch port's
   `#[panic_handler]` delegates to.

Everything else (page tables, IPI plumbing, syscall registration, …)
lives elsewhere. `kernel/core` is the contract those layers meet at.

## Entry contract

The arch port's boot stub (Stage 3) is responsible for:

* zeroing BSS,
* setting up an initial stack,
* parsing the platform's boot protocol (multiboot2 / UEFI / DTB /
  `wasm-bindgen`) into a typed `BootMemoryMap` and `IdentityTableBuilder`,
* constructing a static `log_sink` and `audit_sink`,
* building an `Arc<A: KernelArch>`,
* calling `rustos_kernel_core::kernel_main(boot)`.

`kernel_main` consumes the `BootInfo`, drives the init phases, and
either parks the boot CPU via `KernelArch::halt` on success (Stage 2.7
will replace the trailing halt with the scheduler dispatch loop) or
parks it via `KernelArch::halt` on failure. **The kernel never silently
resets** — that bottom-typed return is the contract (`AGENTS.md` §2).

## Init order

`kernel_main` runs the following phases in this exact order. The order
is the audit contract with external log consumers — re-ordering would
break the boot-timeline they key off (`AGENTS.md` §5.4, §2.4).

| # | Phase   | Subsystem constructed                                                                 |
|---|---------|---------------------------------------------------------------------------------------|
| 0 | —       | `BootStarted` event emitted; `BootInfo::validate` runs.                               |
| 1 | `log`   | `rustos_log::set_max_level(boot.log_level)`.                                          |
| 2 | `mem`   | `rustos_kernel_mem::FrameAllocator::new(&boot.memory_map)`.                           |
| 3 | `sec`   | `boot.identity.verify(boot.audit_sink)` → `IdentityTable`.                            |
| 4 | `sched` | `rustos_kernel_sched::Scheduler::new(boot.scheduler_config, Arc::clone(&boot.arch))`. |
| 5 | `ipc`   | No global state at this stage; the phase event still fires for timeline uniformity.   |
| ∞ | —       | `BootCompleted` event emitted; `arch.halt()` parks the CPU.                           |

Each phase emits exactly:

* one `KERNEL_PHASE_STARTED` record (`EventId(4001)`) with a
  `phase = <name>` field, then
* on success: one `KERNEL_PHASE_READY` record (`EventId(4002)`), or
* on failure: one `KERNEL_PHASE_FAILED` record (`EventId(4003)`)
  carrying both `phase` and `cause` fields, then `arch.halt()`.

The set of stable `cause` strings is enumerated in
`kernel/core/src/init.rs::InitError::cause`.

## Panic policy

The arch port owns the `#[panic_handler]` attribute (host-test builds
cannot define one because `std` already does). It is a one-liner that
delegates to `handle_panic`:

```rust,ignore
#[panic_handler]
fn rustos_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    rustos_kernel_core::handle_panic(info, &PANIC_CTX)
}
```

`PANIC_CTX: PanicContext` is stored in the per-CPU bootstrap area the
arch port owns. Building it once at boot and never mutating it is the
only global-mutable-state exception called out by `AGENTS.md` §2; the
arch port documents it there.

`handle_panic` emits exactly one `KERNEL_PANIC` record (`EventId(4010)`,
`Level::Error`) with the fields below, then calls `KernelArch::halt`.

| Key      | Value                                                |
|----------|------------------------------------------------------|
| `cpu`    | Decimal `KernelArch::current_cpu()`.                 |
| `file`   | `info.location().file()` or `"<unknown>"`.           |
| `line`   | Decimal `info.location().line()` or `"0"`.           |
| `column` | Decimal `info.location().column()` or `"0"`.         |

The handler performs **no allocation**: every formatting buffer is
stack-resident, so the panic path survives a wedged heap.

## `BootInfo` schema

`BootInfo<'a, A: KernelArch>` is `pub`-fielded and consumed by value:

| Field              | Type                              | SAFETY-INVARIANT                              |
|--------------------|-----------------------------------|-----------------------------------------------|
| `boot_cpu`         | `CpuId` (`u32`)                   | `== arch.current_cpu()` at entry.             |
| `cpu_count`        | `u32`                             | `>= 1`, `boot_cpu < cpu_count`.               |
| `command_line`     | `&'a str`                         | `len() <= MAX_COMMAND_LINE_BYTES`.            |
| `memory_map`       | `BootMemoryMap`                   | Usable regions are firmware-released RAM.     |
| `identity`         | `IdentityTableBuilder`            | Verified during the `sec` phase.              |
| `scheduler_config` | `SchedulerConfig`                 | `.cpus == cpu_count`.                         |
| `arch`             | `Arc<A>`                          | Pinned for the lifetime of the running kernel.|
| `log_sink`         | `&'static (dyn Sink + Sync)`      | Lives until power-off.                        |
| `audit_sink`       | `&'static (dyn Sink + Sync)`      | Lives until power-off.                        |
| `log_level`        | `rustos_log::Level`               | Installed before the first `PhaseStarted`.    |

`BootInfo::validate()` runs at the top of `kernel_main` and reports any
violation as a `BootInfoError`; the kernel then logs a `PhaseFailed`
record under the `log` phase and halts.

## Audit event catalogue

`kernel/core` owns the `4_000..5_000` event-id range:

| ID   | Level | Name                  |
|-----:|-------|-----------------------|
| 4000 | Info  | `KERNEL_BOOT_STARTED` |
| 4001 | Info  | `KERNEL_PHASE_STARTED`|
| 4002 | Info  | `KERNEL_PHASE_READY`  |
| 4003 | Error | `KERNEL_PHASE_FAILED` |
| 4004 | Info  | `KERNEL_BOOT_COMPLETED` |
| 4010 | Error | `KERNEL_PANIC`        |

New events take the next free identifier and require an update to this
table and the event catalogue in `kernel/core/src/audit.rs`.

## Testing

The crate is fully host-testable. `kernel/core/tests/kernel_main.rs`
drives the entry point with a `TestArch + TestSink` and asserts:

* the happy-path init order matches this document exactly,
* a failing `mem` phase logs `PhaseFailed { phase = "mem",
  cause = "mem_out_of_memory" }` and halts,
* a malformed `BootInfo` is reported under the `log` phase with the
  documented `cause` string,
* `handle_panic` emits exactly one `KERNEL_PANIC` record and halts.

The mock `TestArch::halt` panics with a sentinel message so
`std::panic::catch_unwind` can observe the halt without blocking the
test runner; this scaffold is gated behind the `test-arch` Cargo
feature and never links into a production build.
