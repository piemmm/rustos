# wasm32

RustOS targets `wasm32-unknown-unknown` as a Tier-1 platform: RustOS in
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
`rustos_arch_api::SchedulerArch` and names only `kernel/arch/api` and
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
| `preempt`       | `requestAnimationFrame` cooperative tick + `MessageChannel` IPI.   |
| `isolation`     | WASM-linear-memory isolation model (the "MMU" analogue).          |
| `syscall_entry` | Host-call argument marshalling + dispatch callback.               |
| `bindings`      | Hand-rolled JS host imports (freestanding wasm only).             |
| `console`       | `console.log`-backed `rustos_log::Sink` (freestanding wasm only). |
| `entry`         | `rustos_arch_wasm32_main` export trampoline (freestanding wasm only). |
| `panic`         | Shared `#[panic_handler]` bridge (freestanding wasm only).        |

### Cooperative scheduling

A WebAssembly module runs to completion on the host's JavaScript turn;
it cannot be pre-empted by a hardware timer. RustOS therefore yields
*cooperatively*. `preempt::init_local_preempt` asks the host for an
animation frame; the host's `requestAnimationFrame` callback re-enters
the exported `rustos_arch_wasm32_on_frame`, which drives one scheduler
tick (`kernel/sched::Scheduler::on_timer_tick`) and requests the next
frame. A directed reschedule to another worker arrives over a
`MessageChannel` post (`WasmArch::send_ipi`) and re-enters the exported
`rustos_arch_wasm32_on_message`.

### WASM-memory isolation

Each Web Worker runs a distinct module instance with its own linear
memory, and the WebAssembly engine bounds-checks every load/store
against that instance's memory — a worker cannot even name another
worker's bytes. The `isolation` module is the architecture-neutral model
of that boundary: a `MemoryRegion` names one worker's span, and an
`AddressSpace` rejects any access outside its region with a `WasmFault`
(the wasm32 equivalent of a page fault). This is the same isolation
guarantee the bare-metal ports get from hardware page tables.

### Hand-rolled host bindings

The host imports in `bindings` are a plain `extern "C"` block resolved
against the WebAssembly `env` import module; the companion glue in
`kernel/arch/wasm32/web/rustos.js` supplies them. RustOS takes no
`wasm-bindgen` / `web-sys` dependency (`AGENTS.md` §2.12): the import set
is tiny, fixed, and audited in one place.

## Host loader

`kernel/arch/wasm32/web/rustos.js` is the JavaScript counterpart of the
bare-metal ports' firmware hand-off. It instantiates a RustOS wasm32
module, supplies the `env` host imports (`performance.now()`, the worker
index, `requestAnimationFrame`, the `MessageChannel` post, and a
`console.log` writer that decodes UTF-8 from the module's linear
memory), and calls the exported `rustos_arch_wasm32_main` once. It is
hand-written and dependency-free, mirroring the no-`wasm-bindgen` policy
of the Rust side.

## Browser-headless harness

The Stage-3 per-sub-stage deliverable — "boots to `init`",
"memory-isolation test passes", "timer interrupt drives the scheduler" —
is exercised by a browser vertical, the wasm32 analogue of the bare-metal
QEMU verticals:

- `tests/integration/kernel_arch_boot_wasm32` is the kernel `cdylib`. Its
  `kernel_main` constructs the `WasmArch` handle (`BOOT_OK`), runs the
  isolation check (`ISOLATION_OK`), and arms the cooperative scheduler;
  the tick callback prints `TICK` each frame.
- `tests/integration/kernel_arch_boot_wasm32/web/harness.mjs` is the
  puppeteer runner. It serves the module and the host loader over a
  loopback HTTP server, boots them in headless Chrome, scrapes the
  console markers, and reports PASS once it has seen `BOOT_OK`,
  `ISOLATION_OK`, and at least twenty `TICK`s. A kernel panic traps the
  instance and surfaces as a page error, failing the run loudly with no
  retries (`AGENTS.md` §7).

## Build and run

The wasm32 vertical is opt-in behind `cargo xtask test --wasm`
(mirroring `test --qemu`), because it needs Node.js, puppeteer, and a
Chrome binary:

```
# Run the host unit tests and the wasm32 browser vertical:
cargo xtask test --wasm

# wasm32 arch-crate host tests only (clock / worker map / cooperative
# scheduler / isolation / syscall marshalling):
cargo test -p rustos-arch-wasm32

# Build the kernel module by hand:
cargo build -p rustos-test-kernel-arch-boot-wasm32 \
    --target wasm32-unknown-unknown

# Run the harness standalone (Chrome path overridable):
node tests/integration/kernel_arch_boot_wasm32/web/harness.mjs \
    --wasm target/wasm32-unknown-unknown/debug/rustos_test_kernel_arch_boot_wasm32.wasm \
    --chrome /usr/bin/google-chrome --timeout-secs 30
```

The harness honours `PUPPETEER_EXECUTABLE_PATH` (or `--chrome`) for the
Chrome binary and defaults to `/usr/bin/google-chrome`.
