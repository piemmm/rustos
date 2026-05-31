# Next session — finish Stage 3c (riscv64), then Stage 3b/3d, then resume Stage 5

## Where we are

Stage 3 (Architecture Ports) had been skipped: only 3a (x86_64) was
complete. Earlier sessions advanced **Stage 3c (riscv64)** with the
per-sub-stage arch primitives below; **this session landed multi-hart
SMP bring-up** (see "Landed this session").

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

## Landed this session — multi-hart SMP bring-up (Stage 3c)

The second of the two remaining 3c items is now done and green:

- `kernel/arch/riscv64/src/sbi.rs` — added the SBI v0.2 **IPI** (sPI)
  `send_ipi` and **HSM** `hart_start` calls returning a typed `SbiRet`,
  plus host-tested extension-id constants and `hart_mask_for`.
- `kernel/arch/riscv64/src/smp.rs` + `smp.s` — `MAX_HARTS`, a
  `tp`-derived `current_hartid`, a set-once secondary-entry callback,
  and `start_secondary` (SBI HSM `hart_start` → the `smp.s` trampoline,
  which seeds each hart's `tp` and a private `.bss` stack slice).
  `boot.s` now also seeds `tp = hartid` on the boot hart.
- `RiscvArch` (`kernel_arch.rs`) — carries a `CpuId`↔hart-id map
  (`new`/`with_harts`/`hartid_of`/`cpu_for_hartid`); `current_cpu`
  reverse-maps the `tp` hart id and `send_ipi` raises a supervisor
  software interrupt on the target hart (replacing the former no-op).
- `preempt.rs` — per-hart timer interval/`CpuId` slots, `enable_ipi`,
  a set-once IPI callback, and `on_software_interrupt` (clears
  `sip.SSIP`, runs the callback); `trap.rs` routes the
  supervisor-software-interrupt cause there.

**QEMU vertical landed:** `tests/integration/ipi_smp_qemu_riscv64`
(`rustos-test-ipi-smp-qemu-riscv64`, enrolled in
`tools/xtask/src/commands/qemu_tests.rs`, **2 CPUs**, 60 s) — boots the
`virt` board, derives the boot hart at runtime (OpenSBI may boot on
either hart), starts the other hart via `smp::start_secondary`, and
after that hart enables interrupts delivers it a directed IPI through
`RiscvArch::send_ipi`; PASS once the secondary hart's `sip.SSIP` trap
path runs the IPI callback with the secondary's id. Verified green via
`cargo xtask ci` (which runs `cargo xtask test --qemu`).

## What needs doing next — finish Stage 3c (riscv64)

The memory-isolation and timer-preempt QEMU verticals plus multi-hart
SMP are all landed and green, so only **one** 3c item remains:

1. **Wire the new primitives into the live scheduler/kernel** — drive a
   real `kernel/sched` `Scheduler::on_timer_tick` from the riscv64
   `preempt` timer callback (and the `smp`/IPI software-interrupt
   callback) in the boot pipeline (`tests/integration/riscv64_boot`),
   and exercise `context::switch` for an actual task switch. The arch
   primitives (`preempt` per-hart timers, `smp` hart start + IPI,
   `paging::AddressSpace`, `context`) are all in place and host/QEMU
   tested; this item connects them to the architecture-neutral
   `Scheduler` rather than the test-local callbacks the verticals use.

3c's per-sub-stage tests ("boots to init", "memory-isolation test
passes", "timer interrupt drives scheduler") and multi-hart SMP are now
all satisfied; the remaining item wires the primitives into the live
kernel scheduler.

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
