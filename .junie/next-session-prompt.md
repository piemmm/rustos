# Next session — Stage 3b/3d, then resume Stage 5

## Where we are

Stage 3 (Architecture Ports) had been skipped: only 3a (x86_64) was
complete. Earlier sessions advanced **Stage 3c (riscv64)** with the
per-sub-stage arch primitives below; **this session landed the final
3c item — wiring the arch primitives into the live `kernel/sched`
scheduler — so Stage 3c is now complete** (see "Landed this session").

### Earlier-session arch primitives
Each host-unit-tested and clippy/rustdoc-clean on both the host and
`riscv64gc-unknown-none-elf`:

- `kernel/arch/riscv64/src/paging.rs` — Sv39 page-table primitives
  (PTE/VPN/`satp` encoders, `.bss` `PageTablePool`, `AddressSpace` with
  gigapage identity map + 4 KiB walk + `satp`/`sfence.vma` `switch`).
- `kernel/arch/riscv64/src/context.rs` + `context.s` — `TaskCtx { sp }`,
  `prepare`, and `rustos_arch_riscv64_switch` (saves `ra`+`s0`–`s11`+`a0`).
- `kernel/arch/riscv64/src/preempt.rs` — supervisor-timer scheduler-tick
  callback, `sie.STIE`, SBI `set_timer` arm/re-arm, `interval_for_hz`.
- `kernel/arch/riscv64/src/syscall_entry.rs` + `trap::TrapFrame` — the
  U-mode `ecall` path (arg marshalling into `rustos_abi`'s
  `[u64; SYSCALL_MAX_ARGS]`, dispatch callback, `sepc += 4`, fail-closed).
- `trap.rs`/`trap.s` extended: the vector now passes a `*mut TrapFrame`;
  the handler routes ecall / supervisor-timer / external causes.

**QEMU vertical landed:** `tests/integration/timer_preempt_qemu_riscv64`
(`rustos-test-timer-preempt-qemu-riscv64`, enrolled in
`tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s) — boots the
`virt` board, arms the timer at 100 Hz, and asserts the supervisor-timer
trap path drives the tick callback ≥ 20 times before `SiFive` PASS. This
is the Stage-3 "timer interrupt drives scheduler" deliverable for
riscv64.

Verified green on this host: `cargo xtask ci` and `cargo xtask test
--qemu` (the full suite, including every riscv64 vertical — the
trap-frame change is non-breaking) both pass.

(Note: `mdbook`/`mdbook-linkcheck`, `cargo-deny`, `cargo-llvm-cov` live
in `~/.cargo/bin`, which is **not** on the default non-interactive
`PATH` — `export PATH="$HOME/.cargo/bin:$PATH"` before invoking the
gate.)

## Landed this session — arch primitives drive the live scheduler (Stage 3c complete)

The last remaining 3c item is now done and green, completing Stage 3c:

- **New QEMU vertical** `tests/integration/sched_drive_qemu_riscv64`
  (`rustos-test-sched-drive-qemu-riscv64`, enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`, **1 CPU**, 60 s). It connects
  the arch `preempt` (timer + IPI) and `context` primitives to the
  architecture-neutral `kernel/sched` `Scheduler`, rather than the
  test-local counting callbacks the `timer_preempt` / `ipi_smp`
  verticals use. On the `virt` board it:
  1. performs a real bidirectional `context::switch` round-trip with
     interrupts disabled — an inbound task seeded by `TaskCtx::prepare`
     records that it ran and `switch`es straight back;
  2. builds a real `rustos-kernel-sched-mlfq::Scheduler` over
     `RiscvArch`, publishes it (leaked `Arc` → `AtomicPtr`), and installs
     **both** the `preempt::set_timer_callback` and
     `preempt::set_ipi_callback` handlers so each drives
     `Scheduler::on_timer_tick`;
  3. arms the 100 Hz SBI timer + IPI (`init_traps`/`enable_ipi`/
     `init_local_preempt`), spawns 64 tasks, sends itself a directed IPI,
     and drives the cooperative `step` loop until every task has run.
  PASS once the supervisor-timer trap has driven the live scheduler ≥ 20
  times and the IPI software-interrupt path has driven it at least once;
  any missing path trips a dedicated `SiFive` failure finisher or times
  out. Verified green via `cargo xtask ci` (which runs
  `cargo xtask test --qemu`).

Note: the live scheduler is driven from this dedicated vertical rather
than `kernel_core::kernel_main` because the latter halts after
`BootCompleted` and keeps its `Scheduler` private (an `init.rs`
internal); mirrors how x86_64's `scheduler_stress_qemu` drives the real
`Scheduler` from the LAPIC-timer ISR.

## What needs doing next — Stage 3b/3d

Stage 3c (riscv64) is **complete**: all per-sub-stage tests ("boots to
init", "memory-isolation test passes", "timer interrupt drives
scheduler"), multi-hart SMP, and the live-scheduler wiring are landed
and green. The next architecture work is:

- **3b `kernel/arch/aarch64`** is a bare 6-line stub: boot stub, UART
  console, MMU, GIC, generic timer, context switch, EL0 syscall entry,
  QEMU `virt` script, and the three per-sub-stage tests.
- **3d `kernel/arch/wasm32`** is a bare 6-line stub: cooperative
  scheduling via `requestAnimationFrame`/`MessageChannel`, WASM-memory
  isolation, browser headless harness.

## Then — resume the earlier Stage 5 follow-ups

(Previously queued, still valid once Stage 3 is complete.)
- Packed virtqueues (virtio 1.1 §2.7) for the virtio transport.
- Interrupt-driven (rather than polled) delivery for the ps2 input
  vertical.
- Begin the Stage 5 deliverables in `PLAN.md`.

## Verification commands

```
export PATH="$HOME/.cargo/bin:$PATH"   # mdbook / cargo-deny / cargo-llvm-cov live here

# Full gate (what a PR must pass):
cargo xtask ci
cargo xtask test --qemu

# Run the riscv64 timer-preempt vertical standalone (fast iteration):
cargo build --locked -p rustos-test-timer-preempt-qemu-riscv64 \
    --target riscv64gc-unknown-none-elf
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-timer-preempt-qemu-riscv64 \
    --arch riscv64 --cpus 1 --timeout-secs 60   # runner exit 0 == PASS

# riscv64 arch-crate host tests (paging/context/preempt/syscall/fault/smp/sbi):
cargo test -p rustos-arch-riscv64

# Run the riscv64 multi-hart IPI/SMP vertical standalone (fast iteration):
cargo build --locked -p rustos-test-ipi-smp-qemu-riscv64 \
    --target riscv64gc-unknown-none-elf
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-ipi-smp-qemu-riscv64 \
    --arch riscv64 --cpus 2 --timeout-secs 60   # runner exit 0 == PASS

# Run the riscv64 memory-isolation vertical standalone (fast iteration):
cargo build --locked -p rustos-test-memory-isolation-qemu-riscv64 \
    --target riscv64gc-unknown-none-elf
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-memory-isolation-qemu-riscv64 \
    --arch riscv64 --cpus 1 --timeout-secs 60   # runner exit 0 == PASS
```
