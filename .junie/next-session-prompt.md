# Next session — Stage 3 complete; resume Stage 5

## Where we are

Stage 3 (Architecture Ports) is now **complete for all four Tier-1
targets**:

- **3a x86_64** — complete.
- **3b aarch64** — complete.
- **3c riscv64** — complete.
- **3d wasm32** — **complete (landed this session)**, see below.

Verified green on this host: the full `cargo xtask` gate
(`fmt`/`clippy`/`cfg-check`/`deps-check`/`docs-check`/`cargo deny`/
`abi-check`), the host test suite, the QEMU matrix (`cargo xtask test
--qemu`), and the new wasm32 browser vertical (`cargo xtask test
--wasm`).

(Note: `mdbook`/`mdbook-linkcheck`, `cargo-deny`, `cargo-llvm-cov` live
in `~/.cargo/bin`, which is **not** on the default non-interactive
`PATH` — `export PATH="$HOME/.cargo/bin:$PATH"` before invoking the
gate. The wasm32 harness needs `node` + `puppeteer` + a Chrome binary;
`/usr/bin/google-chrome` is used by default.)

## Landed this session — Stage 3d (wasm32) complete

`kernel/arch/wasm32` went from a 6-line placeholder to a full Arch HAL
implementation for the browser sandbox (`wasm32-unknown-unknown`),
mirroring the bare-metal ports but mapping every "hardware" concept to a
JavaScript host:

- per-CPU identity → the executing Web Worker context
- monotonic clock → `performance.now()`
- timer-interrupt-drives-scheduler → `requestAnimationFrame` cooperative
  tick
- inter-processor interrupt → a `MessageChannel` post between workers
- MMU / page-table isolation → one WASM linear memory per worker

Modules: `kernel_arch.rs` (`WasmArch` + `ms_to_ns` clock + `CpuId` ↔
worker-index map), `preempt.rs` (rAF cooperative tick + `MessageChannel`
IPI + `cooperative_budget_exhausted`), `isolation.rs` (`MemoryRegion` /
`AddressSpace` / `WasmFault` — the "MMU" analogue), `syscall_entry.rs`
(`pack_raw_args` + set-once dispatch callback), and the
`cfg(target_arch = "wasm32")`-gated browser-host glue: `bindings.rs`
(hand-rolled `extern "C"` `env` imports — no `wasm-bindgen`),
`console.rs` (`console.log` `rustos_log::Sink`), `entry.rs` (the
exported `rustos_arch_wasm32_main` / `on_frame` / `on_message`
trampolines), `panic.rs` (`#[panic_handler]` bridge). Host loader:
`kernel/arch/wasm32/web/rustos.js` (dependency-free). 28 host unit
tests; clippy/rustdoc clean on host + `wasm32-unknown-unknown`.

Browser-headless vertical: `tests/integration/kernel_arch_boot_wasm32`
(a `cdylib` whose `kernel_main` prints `BOOT_OK` / `ISOLATION_OK` /
`TICK`) plus the puppeteer runner `web/harness.mjs`, launched by the new
`cargo xtask test --wasm` (opt-in, mirroring `test --qemu`). The
`rustos-itest-harness` build glue gained an `itest_wasm32` cfg. Docs:
`docs/src/platform/wasm32.md`.

### wasm32 follow-ups (mirror how the bare-metal ports were staged)
- Multi-worker SMP bring-up: spawn real Web Workers and route
  `MessageChannel` IPIs between live module instances (the `send_ipi`
  host post and `on_ipi_message` receive path already exist).
- Wire the `requestAnimationFrame` cooperative tick into the *live*
  `kernel/sched` `Scheduler` (the wasm32 analogue of
  `tests/integration/sched_drive_qemu_riscv64`), and route a real
  user→kernel host call through `syscall_entry::dispatch_syscall`.

## Landed this session — packed virtqueues (virtio 1.1 §2.7)

`lib/virtio` gained a packed-ring sibling to `SplitQueue`:
`kernel`-agnostic `PackedQueue` in `lib/virtio/src/packed.rs`
(single descriptor ring + driver/device event-suppression structs,
in-band `AVAIL`/`USED` flag bits against per-side wrap counters,
`add_chain`/`kick`/`poll_used` reusing the existing `ChainSegment` /
`UsedToken` / `Transport` vocabulary — no transport-interface change).
The in-process peer gained `MockTransport::drain_packed_queue` plus a
packed `PackedRingView` mirroring the split `ring_view`. 11 new tests
(3 helper + 8 end-to-end, incl. ring-wrap-with-reclaim across the ring
boundary) bring `cargo test -p rustos-virtio` to 42 passing; clippy
`-D warnings`, `cargo fmt`, `cargo xtask docs-check / cfg-check /
deps-check`, the `riscv64` no_std build, and all four virtio consumer
crates' tests are green. Docs: new "Packed virtqueue" section in
`docs/src/drivers/virtio.md` (the §2.7 out-of-scope row is removed).

### packed-ring follow-ups
- Negotiate `VIRTIO_F_RING_PACKED` in `virtio_blk` / `virtio_net` and
  pick `PackedQueue` vs `SplitQueue` from the negotiated feature bit
  (the queues are already drop-in compatible at the `ChainSegment` /
  `UsedToken` seam).
- Exercise the packed peer in the QEMU integration verticals once the
  boot-time bus walk (Stage 4.D item 4) lands.

## What needs doing next — resume Stage 5 follow-ups

(Previously queued, still valid now that Stage 3 is complete.)
- Interrupt-driven (rather than polled) delivery for the ps2 input
  vertical.
- Begin the Stage 5 deliverables in `PLAN.md`.

## Verification commands

```
export PATH="$HOME/.cargo/bin:$PATH"   # mdbook / cargo-deny / cargo-llvm-cov live here

# Full gate (what a PR must pass):
cargo xtask ci
cargo xtask test --qemu

# wasm32 browser-headless vertical (boot / isolation / cooperative tick):
cargo xtask test --wasm

# wasm32 arch-crate host tests:
cargo test -p rustos-arch-wasm32

# Run the wasm32 harness standalone (fast iteration):
cargo build -p rustos-test-kernel-arch-boot-wasm32 --target wasm32-unknown-unknown
node tests/integration/kernel_arch_boot_wasm32/web/harness.mjs \
    --wasm target/wasm32-unknown-unknown/debug/rustos_test_kernel_arch_boot_wasm32.wasm \
    --chrome /usr/bin/google-chrome --timeout-secs 30
```
