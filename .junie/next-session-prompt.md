# Continuation Prompt — RustOS Stage 3a (c7-bin)

Copy the text below verbatim into the next agent session as the
`<issue_description>`.

---

Read `AGENTS.md` and `PLAN.md` in full before doing anything else. They
are binding. Do not skim. In particular: §2 (no hacks, no duplication,
no bloat, no interface creep), §7 (tests must pass; no `#[ignore]`;
coverage floors), §10 (`unsafe` requires `// SAFETY:` + a test + safe
encapsulation), §14 (commit format, one logical change per commit), §15
(no stubs, no silenced lints, no weakened security, no invented APIs).

## Context (already on `main`)

Stages 0–2 are complete. **Stage 3a is `in progress`**, with all
per-arch checklist items ticked **except (c7-bin)**:

- **(c7-arch)** — landed in the previous session. The arch crate now
  exposes:
  - `kernel/arch/x86_64::kernel_arch::X86_64Arch` — concrete
    `rustos_kernel_sched::SchedulerArch` impl over the LAPIC ID
    register, RDTSC, and an ephemeral `apic::Lapic` over
    `VolatileLapicMmio` at `preempt::LAPIC_BASE_PHYS` on
    `preempt::TIMER_VECTOR`. Validated constructor
    (`ArchInitError::{BootCpuOutOfRange,BootCpuMissingFromLapicMap,
    BootCpuLapicMismatch}`) with stable cause strings.
  - `kernel/arch/x86_64::kernel_arch::halt() -> !` — `cli; loop { hlt }`
    free function. The `-> !` signature is locked by a compile-time
    `const _: fn() -> ! = halt;` assertion in the module's tests.
  - The module is gated behind an opt-in
    `rustos-arch-x86_64/sched-arch` Cargo feature so the pre-existing
    freestanding Stage-2 QEMU bins continue to link without a
    `#[global_allocator]`. The arch crate's dev-dependencies enable
    `sched-arch` through a self-link so `cargo test` always exercises
    the impl (mirrors the `test-arch` convention used by
    `kernel/sched` and `kernel/core`). Arch-crate host total: 118.
  - `kernel/syscall::table` ships a compile-time
    `const _RAW_ARGS_LAYOUT_MATCHES_ARRAY` block locking
    `RawArgs`'s `#[repr(transparent)]` over `[u64; SYSCALL_MAX_ARGS]`.

- All earlier (c1)..(c6) and (d1) deliverables stand unchanged. See
  `PLAN.md` Stage 3a sub-checklist for the exact list.

## Goal of this session

Land **(c7-bin)** to AGENTS.md quality, then flip Stage 3a to
`complete` in PLAN.md.

Concretely you must land:

1. **New `rustos-kernel` bin crate** (freestanding
   `x86_64-unknown-none`).
   - `Cargo.toml`: `[[bin]] name = "rustos-kernel"`. Enables
     `rustos-arch-x86_64/sched-arch`. Depends on
     `rustos-kernel-core`, `rustos-kernel-mem`, `rustos-kernel-sec`,
     `rustos-kernel-sched`, `rustos-kernel-syscall`, `rustos-log`,
     `rustos-abi`. Linker script reused from
     `kernel/arch/x86_64/linker.ld` via the same `build.rs` pattern as
     `tests/integration/scheduler_stress_qemu/build.rs`.
   - Ships a `#[global_allocator]`. Pick the smallest workable
     allocator — a bump allocator over the post-MB2 usable region is
     fine for boot; document the choice and its limits in the crate
     README. Production heap landing is on a later stage.

2. **`kernel_main(multiboot_info: u64)`** body in the bin crate:
   - Validate the multiboot2 magic via the existing
     `kernel/arch/x86_64::entry` shim.
   - Parse Multiboot2 → ACPI RSDP → MADT → `BootMemoryMap` via the
     Stage 3a (a) primitives.
   - Build `IdentityTableBuilder` (use the existing audited helpers in
     `kernel/sec`) and a `SchedulerConfig`.
   - Construct `X86_64Arch::new(boot_cpu_id, boot_cpu_lapic_id,
     cpu_to_lapic)` from the MADT.
   - **Install** `syscall_entry::set_dispatch_callback` **before**
     enabling `syscall` on any CPU. The callback reinterprets the
     `[u64; SYSCALL_MAX_ARGS]` frame as `RawArgs(arr)` and forwards
     to `rustos_kernel_syscall::Dispatcher::dispatch`.
   - Per-CPU: `percpu::init` → `preempt::init_local_preempt` →
     `syscall_entry::init_local_syscalls`.
   - Build `BootInfo` and forward to
     `rustos_kernel_core::kernel_main`.

3. **`impl KernelArch for X86_64Arch`** in the bin crate.
   `halt` forwards to `kernel_arch::halt()`. The impl lives in the
   bin crate because pulling `rustos-kernel-core` into the arch
   crate would transitively force a `#[global_allocator]` into the
   two pre-existing freestanding Stage-2 QEMU test bins — see the
   note in `kernel/arch/x86_64/Cargo.toml`.

4. **Panic-handler bridge.** The bin crate's `#[panic_handler]`
   delegates to `rustos_kernel_core::handle_panic` with a
   `PanicContext` whose `current_cpu` reads
   `X86_64Arch::current_cpu`. Halt path forwards to
   `kernel_arch::halt`.

5. **QEMU integration test** in
   `tests/integration/kernel_arch_boot/` (or rename — choose the
   smallest extensible path) that:
   - Builds the `rustos-kernel` binary.
   - Boots it under QEMU through the `tools/qemu` runner.
   - Asserts `AuditEvent::BootCompleted` (`EventId 4004`) appears on
     the serial console.
   - Exits with `isa-debug-exit` pass via the existing
     `qemu_exit::exit_success` mechanism — wire a small audit-sink
     observer that flips the QEMU exit on `BootCompleted`.

6. **Tests + docs in the same commit.**
   - New host unit tests for the bin crate's
     `KernelArch::halt` impl (compile-time `-> !` proof, identical
     pattern to (c7-arch)) and for the dispatch-callback bridge
     (the `RawArgs` reinterpretation is host-testable through a
     small `extern "C"` shim).
   - Coverage floors per AGENTS.md §7 stay green: `kernel/arch/x86_64`
     ≥ 85 %; `kernel/mem`, `kernel/sec`, `kernel/ipc`, `lib/caps`,
     `lib/crypto` ≥ 95 %.
   - `docs/src/platform/x86_64.md` gains a (c7-bin) section.
   - `docs/src/architecture/kernel.md` updates the (c7-arch) note
     to point at the now-shipped impl.

7. **PLAN.md update.** Tick (c7-bin); flip (c7) to `[x]`; flip
   Stage 3a to `complete`. Refresh the Stage 2 evidence tail with a
   fresh `cargo xtask ci` quote.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`. If the bin crate +
  `KernelArch` impl together exceed ~600 LOC, split (c7-bin-crate)
  and (c7-qemu-test) into two commits.
- `cargo xtask ci` must be green at HEAD of every commit. Quote the
  tail in the final summary. CI includes the QEMU integration tests
  via `cargo xtask test --qemu` — the new boot-to-init test joins
  that list.
- No `unwrap`/`expect`/`panic!` in production paths. `unsafe` paired
  with `// SAFETY:` and a test or model. No `#[allow(...)]` without a
  justifying comment.
- Docs in the same commit (AGENTS.md §13).
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing.

### Carry-over design notes (still binding)

- `kernel/sync::RwLock` is explicitly process-context-only. Anything
  that runs in interrupt context must avoid the registry `RwLock`,
  the overflow `SpinLock`, and any other process-context-only
  primitive.
- The `define_isr!` macro is the only sanctioned way to emit a
  per-vector ISR stub on x86_64. `syscall`/`sysret` is MSR-driven
  (`IA32_LSTAR`), not an IDT vector; (c6)'s dedicated naked-fn is the
  only exception.
- Callback installation order matters: the trampoline fail-closes via
  `qemu_exit::exit_failure` if it fires before a callback is
  installed. The bin crate must install the dispatch callback
  *before* enabling `syscall` on any CPU.
- `RawArgs(arr)` is the sanctioned bridge from the kernel-stack
  `[u64; SYSCALL_MAX_ARGS]` to the dispatcher; the (c7-arch)
  compile-time layout assertion in `kernel/syscall::table` locks it.

## Toolchain & host requirements (already installed on the workbench)

- `nightly-2026-05-27` toolchain (rustc 1.98.0-nightly).
  PATH: `$HOME/.rustup/toolchains/nightly-2026-05-27-x86_64-unknown-linux-gnu/bin`.
- `qemu-system-x86_64` 8.2.2, `grub-mkrescue`, `xorriso`,
  `/usr/share/OVMF/OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`, `mdbook`,
  `cargo-deny`, `cargo-llvm-cov` (in `~/.cargo/bin`).
- `mdbook` lives in `~/.cargo/bin`. Make sure that directory is on
  `PATH` *before* invoking `cargo xtask ci` — `xtask` does not search
  it automatically.

## Definition of done

- (c7-bin) implemented to AGENTS.md quality.
- A new QEMU integration test boots the `rustos-kernel` binary all
  the way to `kernel_core::AuditEvent::BootCompleted`, with the
  scheduler-tick callback wired and the syscall dispatch callback
  installed before `syscall` is enabled.
- All existing QEMU integration tests continue to pass.
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block (or a new Stage 3a status block if you choose to record it
  separately).
- `PLAN.md` Stage 3a sub-checklist (c7-bin) ticked; (c7) ticked;
  Stage 3a flipped to `complete`.
- One commit per logical change with the AGENTS.md §14 trailer.
