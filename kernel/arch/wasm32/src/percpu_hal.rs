//! wasm32 per-CPU storage ("per-CPU storage").
//!
//! Implements the Arch HAL [`PerCpu`](rustos_arch_api::PerCpu) surface
//! for wasm32. A WebAssembly module has **no per-CPU register** — the
//! bare-metal ports' GS base / `TPIDR_EL1` / `tp` have no analogue. But
//! a "CPU" on this port is a Web Worker context (`crate::kernel_arch`),
//! and each worker runs its **own module instance with its own linear
//! memory**, so a word the instance owns is already private to that
//! worker. This handle is therefore that worker-local slot: no host call
//! is needed, because the isolation is provided by the per-worker
//! instance boundary, not by a shared register the host must partition.
//!
//! The stored word is opaque to this surface (see the
//! [`PerCpu`](rustos_arch_api::PerCpu) trait docs). The same in-handle
//! cell backs both the wasm build (where it is the genuine per-worker
//! slot) and the host build (where it lets the round-trip and isolation
//! conformance verticals run under `cargo test`), so there is one code
//! path and no fake primitive to keep honest.

use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_arch_api::PerCpu;

/// wasm32 implementation of the Arch HAL per-CPU storage surface.
///
/// The per-worker slot lives in this handle. Each Web Worker owns its
/// own module instance and hence its own handle, so the word is private
/// to the worker without any host coordination.
#[derive(Debug, Default)]
pub struct PerCpuStorage {
    /// The worker-local per-CPU word.
    base: AtomicUsize,
}

impl PerCpuStorage {
    /// Construct the wasm32 per-CPU storage handle.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
        }
    }
}

impl PerCpu for PerCpuStorage {
    fn read_self_base(&self) -> usize {
        self.base.load(Ordering::Relaxed)
    }

    unsafe fn write_self_base(&self, base: usize) {
        // No `unsafe` operation is needed on wasm32: the word is a plain
        // worker-local cell, not a register whose write reconfigures the
        // CPU. The method is `unsafe` only to satisfy the one trait every
        // port implements; storing the value here has no wider effect.
        self.base.store(base, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::percpu::conformance;

    #[test]
    fn passes_per_cpu_conformance() {
        conformance::run_all(&PerCpuStorage::new());
        let dynamic: &dyn PerCpu = &PerCpuStorage::new();
        conformance::run_all(dynamic);
    }

    #[test]
    fn per_cpu_word_is_isolated_across_workers() {
        conformance::run_isolation(&PerCpuStorage::new(), &PerCpuStorage::new());
    }
}
