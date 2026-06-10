//! RustOS wasm32 architecture port.
//!
//! Stage 3d brings `kernel/arch/wasm32` from a placeholder to a full
//! Arch HAL implementation for the browser sandbox
//! (`wasm32-unknown-unknown`). It is the structural counterpart of the
//! bare-metal ports (`kernel/arch/{x86_64,aarch64,riscv64}`), but the
//! "hardware" is a JavaScript host rather than a CPU and a chipset:
//!
//! | bare-metal concept            | wasm32 realisation                                    |
//! | ----------------------------- | ----------------------------------------------------- |
//! | per-CPU identity (hart/APIC)  | the executing Web Worker context ([`kernel_arch`])    |
//! | monotonic timer / `time` CSR  | `performance.now()` ([`kernel_arch`])                 |
//! | timer interrupt → scheduler   | `requestAnimationFrame` cooperative tick ([`preempt`])|
//! | inter-processor interrupt     | a `MessageChannel` post between workers ([`preempt`]) |
//! | MMU / page-table isolation    | one WASM linear memory per worker ([`isolation`])     |
//! | `ecall`/`syscall` entry        | a host call carrying number + args ([`syscall_entry`])|
//!
//! # What is here
//!
//! | Module            | Role                                                              |
//! | ----------------- | ----------------------------------------------------------------- |
//! | [`kernel_arch`]   | [`WasmArch`] — the `SchedulerArch` impl + `performance.now()` clock. |
//! | [`preempt`]       | `requestAnimationFrame` cooperative scheduler tick + `MessageChannel` IPI. |
//! | [`isolation`]     | WASM-linear-memory isolation model (the "MMU" analogue).          |
//! | [`syscall_entry`] | Host-call argument marshalling + dispatch callback.               |
//! | `bindings`        | The hand-rolled JS host imports (freestanding wasm only).         |
//! | `console`         | `console.*`-backed `rustos_log::Sink` (freestanding wasm only).   |
//! | `entry`           | `rustos_arch_wasm32_main` export trampoline (freestanding wasm only). |
//! | `panic`           | Shared `#[panic_handler]` bridge (freestanding wasm only).        |
//!
//! # Arch HAL boundary (`AGENTS.md` §17.2 / §17.4)
//!
//! Like the other ports, this crate names only `kernel/arch/api` and
//! `lib/*`, never a concrete kernel subsystem. The browser-host
//! bindings (`bindings`, `console`, `entry`, `panic`) are gated to
//! `cfg(target_arch = "wasm32")`; the [`WasmArch`] handle, the
//! cooperative-scheduler bookkeeping, the [`isolation`] model, and the
//! syscall marshalling build on the host so their unit tests run under
//! `cargo test` without a wasm target (`AGENTS.md` §7). The host
//! substitutes for the JS imports are never linked into a wasm image:
//! the `target_arch = "wasm32"` cfg selects the real bindings there
//! (`AGENTS.md` §1 — no fake primitives in production).
//!
//! # Why hand-rolled JS bindings
//!
//! The host imports in `bindings` are declared as a plain `extern "C"`
//! block resolved against the WebAssembly `env` import module, and the
//! companion glue under `web/rustos.js` provides them. RustOS does not
//! take a `wasm-bindgen` / `web-sys` dependency: that would widen the
//! trusted computing base for a surface this small, against `AGENTS.md`
//! §2.12 ("roll your own; do not trust external code").
#![no_std]
#![deny(missing_docs)]

// The per-worker bookkeeping in [`WasmArch`] is sized from the
// discovered worker count (`AGENTS.md` §24.1), so it lives in an
// allocator-backed boxed slice rather than a fixed array. The
// freestanding wasm image links the binary's `#[global_allocator]`
// (`lib/bumpalloc`); the host test build uses `std`'s allocator below.
extern crate alloc;

// Host unit tests use `std` (e.g. `std::vec::Vec` in fixtures). The
// crate itself stays `no_std` for the freestanding wasm build
// (`AGENTS.md` §1 — no hacks), mirroring the riscv64 port.
#[cfg(test)]
extern crate std;

pub mod isolation;
pub mod kernel_arch;
/// wasm32 implementation of the Arch HAL memory-tagging surface
/// ([`rustos_arch_api::MemoryTagging`], `AGENTS.md` §19.10). WebAssembly
/// exposes no per-granule tagging primitive — spatial safety is the host
/// sandbox's per-worker linear memory — so the port declares it an honest
/// `Unsupported` (see the module docs).
pub mod memtag;
/// wasm32 implementation of the Arch HAL per-CPU storage surface
/// ([`rustos_arch_api::PerCpu`], `AGENTS.md` §17.2): the worker-local
/// slot standing in for the bare-metal ports' per-CPU register (each Web
/// Worker owns its own module instance, so the slot is private to it).
pub mod percpu_hal;
/// wasm32 implementation of the Arch HAL early-boot platform-discovery
/// surface ([`rustos_arch_api::PlatformDiscovery`], `AGENTS.md` §17.2 /
/// §18.2): the host-environment capability query → [`rustos_abi::hwtree`]
/// normalisation.
pub mod platform;
pub mod preempt;
/// wasm32 implementation of the Arch HAL side-channel mitigation
/// surface ([`rustos_arch_api::SideChannelMitigation`], `AGENTS.md`
/// §19.1).
pub mod sidechannel;
/// wasm32 multi-worker (SMP) bring-up: spawn a Web Worker as a secondary
/// logical CPU and recover the running context's id (`AGENTS.md` §17.2 /
/// §4). Kept port-side like the riscv64 / aarch64 ports, not behind an
/// `Smp` Arch HAL trait (`plans/WIRING.md` Stage W6 / W8).
pub mod smp;
pub mod syscall_entry;
/// wasm32 implementation of the Arch HAL timer-programming surface
/// ([`rustos_arch_api::Timer`], `AGENTS.md` §17.2): the architecture-
/// neutral scheduler-tick callback install + dispatch over the
/// cooperative `requestAnimationFrame` loop wired in [`preempt`].
pub mod timer_hal;

#[cfg(target_arch = "wasm32")]
pub mod bindings;
#[cfg(target_arch = "wasm32")]
pub mod console;
#[cfg(target_arch = "wasm32")]
pub mod entry;
#[cfg(target_arch = "wasm32")]
pub mod panic;

pub use kernel_arch::WasmArch;

#[cfg(target_arch = "wasm32")]
pub use console::{ConsoleSink, CONSOLE_SINK};
#[cfg(target_arch = "wasm32")]
pub use panic::handle_panic_via_console;
