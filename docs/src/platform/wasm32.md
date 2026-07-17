# wasm32

TAIRiX targets `wasm32-unknown-unknown` as a Tier-1 platform: TAIRiX in
a browser. Stage 3d brings `kernel/arch/wasm32` from a placeholder to a
full Arch HAL implementation. It is the structural counterpart of the
bare-metal ports (`x86_64`, `aarch64`, `riscv64`), but the "hardware" is
a JavaScript host rather than a CPU and a chipset, so the realisations
differ:

| bare-metal concept           | wasm32 realisation                              |
| ---------------------------- | ----------------------------------------------- |
| per-CPU identity (hart/APIC) | the executing Web Worker context                |
| monotonic timer / `time` CSR | `performance.now()`                             |
| timer interrupt → scheduler  | `requestAnimationFrame` cooperative tick        |
| inter-processor interrupt    | a `MessageChannel` post between workers          |
| MMU / page-table isolation   | one WASM linear memory per worker                |
| `ecall` / `syscall` entry    | a host call carrying a number + argument array  |

## Arch HAL boundary

Like the bare-metal ports, `kernel/arch/wasm32` is a pure Arch HAL
implementation (`AGENTS.md` §17.2 / §17.4): `WasmArch` implements
`tairix_arch_api::SchedulerArch` and names only `kernel/arch/api` and
`lib/*`, never a concrete kernel subsystem. Where a bare-metal port
gates its CSR/assembly to `cfg(all(target_arch = "...", target_os =
"none"))`, the wasm32 port gates its browser-host bindings (the imported
JS functions, the console sink, the panic bridge, and the exported entry
trampoline) to `cfg(target_arch = "wasm32")`. Everything else — the
`WasmArch` handle, the cooperative-scheduler bookkeeping, the
WASM-memory isolation model, and the syscall-argument marshalling —
builds on the host, so its unit tests run under `cargo test` without a
wasm target.

## Modules

| Module          | Role                                                              |
| --------------- | ----------------------------------------------------------------- |
| `kernel_arch`   | `WasmArch` — the `SchedulerArch` impl + `performance.now()` clock. |
| `percpu_hal`    | `PerCpuStorage` — the `PerCpu` per-CPU storage slice (worker slot). |
| `preempt`       | `requestAnimationFrame` cooperative tick + `MessageChannel` IPI.   |
| `smp`           | Multi-worker bring-up: spawn a Web Worker as a secondary CPU.      |
| `timer_hal`     | `TimerHal` — the `Timer` timer-programming slice (tick dispatch).  |
| `isolation`     | WASM-linear-memory isolation model (the "MMU" analogue).          |
| `syscall_entry` | Host-call argument marshalling + dispatch callback.               |
| `bindings`      | Hand-rolled JS host imports (freestanding wasm only).             |
| `console`       | `console.log`-backed `tairix_log::Sink` (freestanding wasm only). |
| `entry`         | `tairix_arch_wasm32_main` export trampoline (freestanding wasm only). |
| `panic`         | Shared `#[panic_handler]` bridge (freestanding wasm only).        |

### Cooperative scheduling

A WebAssembly module runs to completion on the host's JavaScript turn;
it cannot be pre-empted by a hardware timer. TAIRiX therefore yields
*cooperatively*. `preempt::init_local_preempt` asks the host for an
animation frame; the host's `requestAnimationFrame` callback re-enters
the exported `tairix_arch_wasm32_on_frame`, which drives one scheduler
tick (`kernel/sched::Scheduler::on_timer_tick`) and requests the next
frame. A directed reschedule to another worker arrives over a
`MessageChannel` post (`WasmArch::send_ipi`) and re-enters the exported
`tairix_arch_wasm32_on_message`.

The tick and IPI callbacks drive a *live* `kernel/sched` scheduler — the
same `tairix-kernel-sched-mlfq::Scheduler` the bare-metal ports run
(`plans/WIRING.md` Stage W8). On the main thread the
`requestAnimationFrame` loop drives `Scheduler::on_timer_tick` and
dispatches a ready task with `Scheduler::step` each frame; a delivered
cross-context IPI does the same on the receiving worker. A dedicated Web
Worker has no `requestAnimationFrame`, so a worker drives its cooperative
tick from `setTimeout` instead — the kernel side (`request_frame`) is
identical.

### Multi-worker SMP (`smp`)

The boot context is the main thread, logical CPU 0. The `smp` module is
the wasm32 analogue of the bare-metal ports' secondary-core bring-up
(`kernel/arch/riscv64::smp` over SBI HSM, `kernel/arch/aarch64::smp` over
PSCI `CPU_ON`). `smp::start_worker(n)` range-checks `n` against the
spawnable secondary range (`1..MAX_WORKERS`) and asks the host to spawn a
real Web Worker that instantiates this same module as logical CPU `n`,
with its own linear memory; `smp::current_worker` recovers the running
context's logical id. An out-of-range index or a host refusal fails
closed with a `StartWorkerError` (`AGENTS.md` §2.9). Because each worker
is a separate module instance with its own scheduler and heap, an
inter-context IPI is the only path between them; the main thread is the
routing hub for worker→worker posts.

Bring-up is now reached through the Arch HAL `SecondaryBringup` slice
(`tairix_arch_api::smp`, `plans/WIRING.md` Stage W14):
`WasmArch::start_secondary(cpu)` resolves the dense `CpuId` to its worker
index through the handle's map (failing closed with `SmpError::InvalidCpu`
for the boot context or an unmapped id) and delegates to
`smp::start_worker`. wasm32 has no settable entry pointer — a secondary
is a fresh module instance entering at a fixed export — so it never
reports `NotReady`. The host `passes_secondary_bringup_conformance` test
runs `smp::conformance::run_all` over a real `WasmArch`; the real Web
Worker spawn is proven by the wasm32 browser vertical, which since
`plans/WIRING.md` Stage W15 spawns its worker through this HAL trait
(`arch.start_secondary(WORKER_CPU)`) rather than calling the port-private
`smp::start_worker` directly. With this slice every §17.2 primitive is
behind the HAL.

### Timer programming (`Timer`)

The `timer_hal` module implements the Arch HAL `Timer` slice (`AGENTS.md`
§17.2 / `plans/WIRING.md` Stage W4). `TimerHal::set_tick_callback` /
`tick_callback` forward to the `preempt` cooperative-tick callback static,
and `dispatch_tick` invokes it. `preempt::on_animation_frame` dispatches
each frame's tick through `TimerHal::dispatch_tick`, so the callback
invoke lives in one place (§2.2); requesting the next animation frame
stays in `preempt` (§2.4). Because `on_animation_frame` is host-callable,
the handle is zero-sized and forwards to the same `preempt` static on
both the wasm target and the host build (rather than keeping a private
host cell), so the `passes_timer_conformance` host test runs
`timer::conformance::run_all` over a real `TimerHal`; it shares the
`preempt` host-test serialisation lock so the suites do not race.

### WASM-memory isolation

Each Web Worker runs a distinct module instance with its own linear
memory, and the WebAssembly engine bounds-checks every load/store
against that instance's memory — a worker cannot even name another
worker's bytes. The `isolation` module is the architecture-neutral model
of that boundary: a `MemoryRegion` names one worker's span, and an
`AddressSpace` rejects any access outside its region with a `WasmFault`
(the wasm32 equivalent of a page fault). This is the same isolation
guarantee the bare-metal ports get from hardware page tables.

The browser vertical's isolation check is **per worker** and tied to real
memory: `isolation::live_memory_region` reads this instance's actual
linear-memory size from the engine (`memory.size` × the 64 KiB WASM
page), and every context — the main thread and each spawned worker —
builds an `AddressSpace` over its own memory, confirms a live in-bounds
address is owned, and confirms an attacker confined to a disjoint region
(standing in for another worker's separate linear memory) faults on it.

### Per-CPU storage (worker slot)

The `percpu_hal` module implements the Arch HAL `PerCpu` slice
(`AGENTS.md` §17.2). A WebAssembly module has no per-CPU register, but a
"CPU" here is a Web Worker context and each worker runs its **own module
instance with its own linear memory**, so a word the instance owns is
already private to that worker. `PerCpuStorage` is therefore that
worker-local slot — `read_self_base` / `write_self_base` load/store an
in-handle cell, with no host call, because the per-worker instance
boundary provides the isolation a shared register would otherwise need
the host to partition. The same cell backs the host build, so the
round-trip + isolation conformance verticals (`percpu::conformance`,
folded into `passes_arch_hal_conformance_suite`) run under `cargo test`.

### Context switch (`ContextSwitch`) — n/a

The Arch HAL `ContextSwitch` slice (`AGENTS.md` §17.2 / `plans/WIRING.md`
Stage W5) is **not applicable** to wasm32 and the port implements no
`ContextSwitchHal`. A context switch saves and restores a task's CPU
register state on its kernel stack, but a WebAssembly module has no
addressable register file or stack pointer to swap, and each "CPU" is a
separate Web Worker running its own module instance that the kernel never
swaps register state under — concurrency is the cooperative
`requestAnimationFrame` tick plus `MessageChannel` posts, not a register
swap. Synthesising a fake switch the host cannot perform would be a fake
primitive (`AGENTS.md` §2.1), so the slice is honestly absent here, the
same shape as the missing paging/user-entry primitives.

### MMU / page-table (`AddressSpace`) — n/a

The Arch HAL `AddressSpace` slice (`AGENTS.md` §17.2 / `plans/WIRING.md`
Stage W5b-1) is **not applicable** to wasm32 and the port implements no
`AddressSpace`: there is no page table to program. A WebAssembly module's
address space is a single linear memory the host (the browser engine)
owns and bounds-checks; the kernel has no `CR3`/`satp`/`TTBR0_EL1` to
load and no leaf entries to encode, and isolation between "CPUs" is the
separate Web Worker module instances, not page-table divergence. Mapping
a virtual page to a physical frame has no meaning under the sandbox, so
synthesising a `map_page`/`activate` the host cannot perform would be a
fake primitive (`AGENTS.md` §2.1). The slice is honestly absent, the same
shape as the missing context-switch/user-entry primitives, which is why
wasm32 has no `memory_isolation` QEMU vertical.

### Cross-CPU TLB shootdown (`CrossCpuTlbShootdown`) — n/a

The Arch HAL `CrossCpuTlbShootdown` slice (`AGENTS.md` §17.2 /
`plans/WIRING.md` Stage W13) is **not applicable** to wasm32 and the port
implements no `CrossCpuTlbShootdown`. A cross-CPU shootdown invalidates a
stale page translation cached in *other* CPUs' TLBs, but each wasm32 "CPU"
is a separate Web Worker with its own linear-memory module instance — no
shared page table, and no software-visible TLB to invalidate (the engine
bounds-checks accesses against the instance's own memory). There is
nothing to shoot down, so synthesising the call would be a fake primitive
(`AGENTS.md` §2.1); it is honestly absent, the same shape as the missing
MMU/context-switch/user-entry primitives. This is why only the three
bare-metal ports carry a `cross_cpu_tlb_shootdown_qemu_*` vertical.

### Hand-rolled host bindings

The host imports in `bindings` are a plain `extern "C"` block resolved
against the WebAssembly `env` import module; the companion glue in
`kernel/arch/wasm32/web/tairix.js` supplies them. TAIRiX takes no
`wasm-bindgen` / `web-sys` dependency (`AGENTS.md` §2.12): the import set
is tiny, fixed, and audited in one place.

## Host loader

`kernel/arch/wasm32/web/tairix.js` is the JavaScript counterpart of the
bare-metal ports' firmware hand-off. It instantiates a TAIRiX wasm32
module, supplies the `env` host imports (`performance.now()`, the worker
index, `requestAnimationFrame`, the `MessageChannel` post, the Web Worker
spawn, a `console.log` writer that decodes UTF-8 from the module's linear
memory, and a framebuffer-present writer that paints the module's
RGBA8888 surface onto a canvas and reads it back), and calls the exported
`tairix_arch_wasm32_main` once. It
is hand-written and dependency-free, mirroring the no-`wasm-bindgen`
policy of the Rust side.

The loader also owns the multi-worker SMP plumbing. `boot` runs the
module on the main thread as CPU 0; when the kernel calls
`tairix_host_start_worker(n)`, the main thread spawns a real module Web
Worker (`kernel/arch/wasm32/web/worker.js`, which re-uses the shared
`instantiate`/`runWorker` logic) joined to the main thread by a
`MessageChannel`. An inter-context IPI (`tairix_host_post_ipi`) is a post
on that channel that re-enters the target's `tairix_arch_wasm32_on_message`;
the main thread routes worker→worker posts.

## Browser-headless harness

The deliverables — "boots to `init`", "per-worker memory isolation",
"a live `kernel/sched` scheduler driven by the frame tick", and (Stage
W8) "multi-worker SMP + cross-context IPI" — are exercised by a browser
vertical, the wasm32 analogue of the bare-metal QEMU verticals:

- `tests/integration/kernel_arch_boot_wasm32` is the kernel `cdylib`. Its
  `kernel_main` runs the per-worker isolation check (`ISOLATION_OK`) in
  every context and branches on the logical CPU id. CPU 0 prints
  `BOOT_OK`, builds a live `Scheduler<WasmArch>`, arms the
  `requestAnimationFrame` loop that drives it (`TICK` per frame), spawns
  a real Web Worker as CPU 1, and sends it a directed IPI. CPU 1 prints
  `WORKER_OK`, builds its own live scheduler, and prints `IPI_RECV` when
  the cross-context `MessageChannel` IPI drives it.
- `tests/integration/kernel_arch_boot_wasm32/web/harness.mjs` is the
  puppeteer runner. It serves the module, the host loader, and the worker
  bootstrap over a loopback HTTP server, boots them in headless Chrome,
  scrapes the console markers (workers post theirs to the main thread for
  the page console), and reports PASS once it has seen `BOOT_OK`,
  `ISOLATION_OK`, `WORKER_OK`, `IPI_RECV`, and at least twenty `TICK`s. A
  kernel panic traps the instance and surfaces as a page error, failing
  the run loudly with no retries (`AGENTS.md` §7).

### Display vertical (browser canvas)

The `display` row of the parity matrix (`plans/WIRING.md` Stage W16) is a
second browser vertical, the wasm32 analogue of
`framebuffer_display_qemu_{riscv64,aarch64}`:

- `tests/integration/framebuffer_display_wasm32` is a kernel `cdylib`
  that, on the boot context, loads the build-time signed framebuffer
  display `.rxe` through `tairix_drvhost::Host` (the §8 load gate) and
  drives `load → use → unload → reload`. "Use" maps a static RGBA8888
  surface through a capability-checked `WasmMmioMapper` — the MMU-less
  analogue of the kernel MMIO mapper: there is no page table, so a
  "register window" is a bounds- and `CAP_MMIO_MAP`-gated view of the one
  surface this instance owns — and `present`s a frame. Each presented
  frame is confirmed **twice**: through a second, independently-mapped
  window (the bytes reached linear memory) and through the new
  `tairix_host_present_framebuffer` host import, which paints the surface
  onto a canvas and returns the count of pixels that survived the canvas
  round-trip (it must equal `WIDTH × HEIGHT`). It prints `BOOT_OK` then
  `DISPLAY_OK`; any failure traps the instance (`AGENTS.md` §2.9).
- `tests/integration/framebuffer_display_wasm32/web/index.html` supplies
  the real `presentFramebuffer` hook backed by an on-page `<canvas>`;
  `web/harness.mjs` is the boot harness's sibling and reports PASS once it
  has seen `BOOT_OK` and `DISPLAY_OK`.

Both verticals are enrolled in one `VERTICALS` list in
`tools/xtask/src/commands/wasm_tests.rs`, so `cargo xtask test --wasm`
builds and runs both; adding a wasm32 vertical is one row there
(`AGENTS.md` §2.2).

## Build and run

The wasm32 verticals are opt-in behind `cargo xtask test --wasm`
(mirroring `test --qemu`), because it needs Node.js, puppeteer, and a
Chrome binary:

```
# Run the host unit tests and the wasm32 browser vertical:
cargo xtask test --wasm

# wasm32 arch-crate host tests only (clock / worker map / cooperative
# scheduler / isolation / syscall marshalling):
cargo test -p tairix-arch-wasm32

# Build the kernel module by hand:
cargo build -p tairix-test-kernel-arch-boot-wasm32 \
    --target wasm32-unknown-unknown

# Run the harness standalone (Chrome path overridable):
node tests/integration/kernel_arch_boot_wasm32/web/harness.mjs \
    --wasm target/wasm32-unknown-unknown/debug/tairix_test_kernel_arch_boot_wasm32.wasm \
    --chrome /usr/bin/google-chrome --timeout-secs 30
```

The harness honours `PUPPETEER_EXECUTABLE_PATH` (or `--chrome`) for the
Chrome binary and defaults to `/usr/bin/google-chrome`.
