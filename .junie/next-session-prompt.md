# Next session — Stage 3d (wasm32), then resume Stage 5

## Where we are

Stage 3 (Architecture Ports) is now complete for **three of four**
Tier-1 targets:

- **3a x86_64** — complete.
- **3b aarch64** — **complete (landed this session)**, see below.
- **3c riscv64** — complete.
- **3d wasm32** — still a bare 6-line stub. This is the next
  architecture deliverable.

Verified green on this host: `cargo xtask ci` and `cargo xtask test
--qemu` (the full suite, including all three new aarch64 verticals).

(Note: `mdbook`/`mdbook-linkcheck`, `cargo-deny`, `cargo-llvm-cov` live
in `~/.cargo/bin`, which is **not** on the default non-interactive
`PATH` — `export PATH="$HOME/.cargo/bin:$PATH"` before invoking the
gate.)

## Landed this session — Stage 3b (aarch64) complete

`kernel/arch/aarch64` went from a placeholder to a full Arch HAL
implementation for the QEMU `virt` board, mirroring the riscv64 port.
Pure bit/encoding/layout math is host-unit-tested (39 host tests),
clippy/rustdoc clean on host + `aarch64-unknown-none`; the boot /
console / exception / GIC / timer / MMU system-register and assembly
operations are gated to `cfg(all(target_arch = "aarch64", target_os =
"none"))`.

Modules: `boot.s` (EL2→EL1 drop, stack, `.bss` zero, DTB hand-off),
`entry.rs`, `serial.rs` (PL011 UART `Sink`), `panic.rs`, `qemu_exit.rs`
(ARM semihosting `SYS_EXIT`), `kernel_arch.rs` (`Aarch64Arch` +
`CNTPCT`/`CNTFRQ` clock), `paging.rs` (stage-1 4 KiB / 3-level MMU,
`T0SZ=25` 39-bit VA — the Sv39 mirror), `context.rs`/`context.s`
(AAPCS64 task switch), `preempt.rs` (EL1 physical timer + GIC PPI 30),
`vectors.s` + `exceptions.rs` (EL1 vector table + IRQ/sync dispatch),
`gic.rs` (GICv2 driver), `syscall_entry.rs` (`svc` marshalling),
`fault.rs` (`ESR_EL1` abort hook). Linker: `link/aarch64-virt.ld`.

Three Stage-3 per-sub-stage QEMU verticals, enrolled in
`tools/xtask/src/commands/qemu_tests.rs` (single CPU, 60 s) and verified
green under QEMU:

- `tests/integration/kernel_arch_boot_aarch64` — boots to init.
- `tests/integration/timer_preempt_qemu_aarch64` — the GICv2 timer PPI
  drives the `preempt` callback ≥ 20 times.
- `tests/integration/memory_isolation_qemu_aarch64` — an attacker
  `AddressSpace` faults on a victim-only page.

Host runner: new `Arch::Aarch64` + `tools/qemu/src/aarch64.rs` (`virt`,
`cortex-a72`, semihosting result protocol) + `Spec::for_aarch64_kernel`;
the integration harness gained the `itest_aarch64` cfg; docs in
`docs/src/platform/aarch64.md`.

### aarch64 follow-ups (mirrors how riscv64 was staged)
- Multi-hart SMP bring-up: `MPIDR_EL1` → dense `CpuId` map and
  secondary-core start (PSCI `CPU_ON`). `send_ipi` already raises a
  GICv2 SGI.
- Wire the `paging::AddressSpace` / `context::switch` primitives into the
  *live* `kernel/sched` `Scheduler` (the aarch64 analogue of
  `tests/integration/sched_drive_qemu_riscv64`), and route the live EL0
  `svc` register frame through to `syscall_entry::dispatch_svc`.

## What needs doing next — Stage 3d (wasm32)

- **3d `kernel/arch/wasm32`** is a bare 6-line stub: cooperative
  scheduling via `requestAnimationFrame`/`MessageChannel`, WASM-memory
  isolation between worker contexts, and a browser headless harness.
  This is structurally different from the bare-metal ports (no QEMU; a
  browser/`wasm32-unknown-unknown` test environment), so plan the test
  harness first.

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

# aarch64 arch-crate host tests (paging/context/preempt/syscall/fault/gic/kernel_arch):
cargo test -p rustos-arch-aarch64

# Run an aarch64 vertical standalone (fast iteration):
cargo build --locked -p rustos-test-timer-preempt-qemu-aarch64 \
    --target aarch64-unknown-none
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/aarch64-unknown-none/debug/rustos-test-timer-preempt-qemu-aarch64 \
    --arch aarch64 --cpus 1 --timeout-secs 60   # runner exit 0 == PASS

# riscv64 verticals (unchanged, still green):
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-timer-preempt-qemu-riscv64 \
    --arch riscv64 --cpus 1 --timeout-secs 60
```
