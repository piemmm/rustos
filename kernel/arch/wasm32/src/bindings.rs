//! Hand-rolled JavaScript host imports for the wasm32 port.
//!
//! These are the wasm32 analogue of the bare-metal ports' privileged
//! instructions: where riscv64 issues an `ecall` to OpenSBI or reads a
//! CSR, the wasm32 port calls out to its JavaScript host. The functions
//! are declared as a plain `extern "C"` block, so the WebAssembly module
//! imports them from the default `env` module; the companion glue under
//! `web/rustos.js` supplies them.
//!
//! RustOS deliberately does **not** depend on `wasm-bindgen` / `web-sys`
//! for this surface (`AGENTS.md` §2.12): the import set is tiny, fixed,
//! and audited here in one place, so wrapping it in a third-party
//! binding generator would only widen the trusted computing base.
//!
//! Each import has a single safe wrapper below. The raw `extern` block
//! is private; the rest of the crate only ever calls the wrappers, so
//! the `unsafe` of a host call never leaks across a module boundary
//! (`AGENTS.md` §2.10).

// Resolve every host import against the WebAssembly `env` module — the
// module name the companion glue (`web/rustos.js`) supplies them under.
// Declaring it explicitly keeps the import names stable regardless of
// toolchain defaults.
#[link(wasm_import_module = "env")]
extern "C" {
    /// Monotonic wall-clock reading in fractional milliseconds, as the
    /// host's `performance.now()` returns. Never decreases within a
    /// single document context.
    fn rustos_host_now_ms() -> f64;

    /// Identifier of the Web Worker context currently executing this
    /// module instance. The boot context is `0`; each spawned worker
    /// receives a distinct, dense index from the host.
    fn rustos_host_current_worker() -> u32;

    /// Ask the host to deliver a cooperative reschedule to `worker` by
    /// posting on its `MessageChannel`. Best-effort: an unknown worker
    /// index is dropped by the host.
    fn rustos_host_post_ipi(worker: u32);

    /// Ask the host to schedule one `requestAnimationFrame` callback,
    /// which re-enters [`crate::preempt::on_animation_frame`]. Idempotent
    /// within a frame on the host side.
    fn rustos_host_request_frame();

    /// Emit `len` bytes starting at `ptr` to the host console
    /// (`console.log`). The bytes are UTF-8; the host decodes them.
    fn rustos_host_console_write(ptr: *const u8, len: usize);
}

/// Read the host monotonic clock in fractional milliseconds.
#[must_use]
pub fn host_now_ms() -> f64 {
    // SAFETY: `rustos_host_now_ms` is a pure host import with no
    // pointer arguments and no side effects; the glue guarantees it
    // returns a finite, non-decreasing `performance.now()` reading.
    unsafe { rustos_host_now_ms() }
}

/// Identifier of the executing Web Worker context.
#[must_use]
pub fn host_current_worker() -> u32 {
    // SAFETY: `rustos_host_current_worker` is a pure host import taking
    // no arguments; the glue returns the dense worker index it assigned.
    unsafe { rustos_host_current_worker() }
}

/// Request a cooperative reschedule on `worker`.
pub fn host_post_ipi(worker: u32) {
    // SAFETY: `rustos_host_post_ipi` takes a plain integer and has no
    // memory side effects in this module; an unknown index is a host
    // no-op (best-effort delivery, mirroring a dropped hardware IPI).
    unsafe { rustos_host_post_ipi(worker) }
}

/// Schedule one animation-frame scheduler tick.
pub fn host_request_frame() {
    // SAFETY: `rustos_host_request_frame` takes no arguments and only
    // registers a host callback; it has no memory side effects here.
    unsafe { rustos_host_request_frame() }
}

/// Emit `bytes` to the host console.
pub fn host_console_write(bytes: &[u8]) {
    // SAFETY: `bytes` is a live slice for the duration of the call, so
    // `(ptr, len)` names exactly its valid range; the host copies the
    // bytes out synchronously and retains no pointer past return.
    unsafe { rustos_host_console_write(bytes.as_ptr(), bytes.len()) }
}
