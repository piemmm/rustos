# Next session — finish Stage 3c (riscv64), then Stage 3b/3d, then resume Stage 5

## Where we are

Stage 3 (Architecture Ports) had been skipped: only 3a (x86_64) was
complete. This session advanced **Stage 3c (riscv64)** by landing the
remaining per-sub-stage **arch primitives**, each host-unit-tested and
clippy/rustdoc-clean on both the host and `riscv64gc-unknown-none-elf`:

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

## What needs doing next — finish Stage 3c (riscv64)

The **memory-isolation QEMU vertical** is now landed and green:
`tests/integration/memory_isolation_qemu_riscv64` builds a victim and
an attacker Sv39 `paging::AddressSpace` that disagree on one VA, installs
a handler through the new `kernel/arch/riscv64::fault` hook, `switch`es
`satp` to the attacker, and confirms the MMU raises a load page fault on
the isolated VA while the victim frame stays intact before writing the
`SiFive` Test PASS finisher (the riscv64 analogue of
`tests/integration/memory_isolation`; `AGENTS.md` §4). That satisfies
the Stage-3 "memory-isolation test passes" deliverable for riscv64. The
`fault` module adds a set-once `FaultHandlerFn(scause, stval, sepc) -> !`
that the `trap` handler invokes for an unexpected synchronous exception
before falling back to parking the hart.

Remaining for 3c:

1. **Wire the new primitives into the live scheduler/kernel** — drive
   real `Scheduler::on_timer_tick` from the riscv64 `preempt` callback
   in the boot pipeline, and exercise `context::switch` for an actual
   task switch.
2. **Multi-hart SMP bring-up** — SBI `IPI` delivery (replacing the
   documented `RiscvArch::send_ipi` single-hart no-op), per-hart timer
   intervals, and `tp`-derived hart identity.

3c's per-sub-stage tests ("boots to init", "memory-isolation test
passes", "timer interrupt drives scheduler") are now all satisfied; the
remaining items above wire the primitives into the live kernel and add
multi-hart SMP.

## Then — Stage 3b/3d

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

# riscv64 arch-crate host tests (paging/context/preempt/syscall/fault):
cargo test -p rustos-arch-riscv64

# Run the riscv64 memory-isolation vertical standalone (fast iteration):
cargo build --locked -p rustos-test-memory-isolation-qemu-riscv64 \
    --target riscv64gc-unknown-none-elf
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-memory-isolation-qemu-riscv64 \
    --arch riscv64 --cpus 1 --timeout-secs 60   # runner exit 0 == PASS
```
