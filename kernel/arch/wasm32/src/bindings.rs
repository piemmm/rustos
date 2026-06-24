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
//! for this surface: the import set is tiny, fixed,
//! and audited here in one place, so wrapping it in a third-party
//! binding generator would only widen the trusted computing base.
//!
//! Each import has a single safe wrapper below. The raw `extern` block
//! is private; the rest of the crate only ever calls the wrappers, so
//! the `unsafe` of a host call never leaks across a module boundary.

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

    /// Ask the host to spawn a new Web Worker as logical CPU `worker`,
    /// instantiating this same module in it. Returns `1` if the host
    /// started the worker, `0` if it refused (an out-of-range index, a
    /// duplicate, or a context that cannot spawn workers). The wasm32
    /// analogue of the bare-metal ports' secondary-core bring-up (PSCI
    /// `CPU_ON` / SBI HSM `hart_start`).
    fn rustos_host_start_worker(worker: u32) -> u32;

    /// Ask the host to schedule one `requestAnimationFrame` callback,
    /// which re-enters [`crate::preempt::on_animation_frame`]. Idempotent
    /// within a frame on the host side.
    fn rustos_host_request_frame();

    /// Emit `len` bytes starting at `ptr` to the host console
    /// (`console.log`). The bytes are UTF-8; the host decodes them.
    fn rustos_host_console_write(ptr: *const u8, len: usize);

    /// Number of logical processors the host advertises
    /// (`navigator.hardwareConcurrency`), the wasm32 analogue of the
    /// CPU count a bare-metal port reads from ACPI/FDT. At least `1`.
    fn rustos_host_logical_processors() -> u32;

    /// Whether the host environment exposes a display surface (a canvas)
    /// the desktop could render to: `1` if present, `0` otherwise.
    fn rustos_host_has_display() -> u32;

    /// Present `len` bytes of a `width`×`height` RGBA8888 framebuffer
    /// surface, beginning at `ptr` in this module's linear memory and
    /// with `stride` bytes between scanlines, to the host display
    /// surface (a canvas). The host copies the bytes out synchronously
    /// (it retains no pointer past return), paints them onto the canvas,
    /// reads the painted region back, and returns the number of pixels
    /// that survived the canvas round-trip unchanged — the wasm32
    /// scan-out analogue of a bare-metal port reading its framebuffer
    /// back through an independent mapping. A host with no display
    /// surface returns `0`.
    fn rustos_host_present_framebuffer(
        ptr: *const u8,
        len: usize,
        width: u32,
        height: u32,
        stride: u32,
    ) -> u32;
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

/// Number of logical processors the host advertises (at least `1`).
#[must_use]
pub fn host_logical_processors() -> u32 {
    // SAFETY: a pure host import taking no arguments; the glue returns
    // `navigator.hardwareConcurrency` clamped to at least 1.
    unsafe { rustos_host_logical_processors() }
}

/// Ask the host to spawn a new Web Worker as logical CPU `worker`.
///
/// Returns `true` if the host started the worker, `false` if it refused.
#[must_use]
pub fn host_start_worker(worker: u32) -> bool {
    // SAFETY: `rustos_host_start_worker` takes a plain integer and has no
    // memory side effects in this module; the host validates the index
    // and returns a non-zero status only when it started the worker.
    unsafe { rustos_host_start_worker(worker) != 0 }
}

/// Whether the host exposes a display surface.
#[must_use]
pub fn host_has_display() -> bool {
    // SAFETY: a pure host import taking no arguments; the glue returns
    // `1` when a canvas display is available, `0` otherwise.
    unsafe { rustos_host_has_display() != 0 }
}

/// Present the RGBA8888 framebuffer `frame` (a `width`×`height` surface
/// with `stride` bytes per scanline) to the host display surface.
///
/// Returns the number of pixels the host confirmed survived the canvas
/// round-trip unchanged; `0` if the host exposes no display.
#[must_use]
pub fn host_present_framebuffer(frame: &[u8], width: u32, height: u32, stride: u32) -> u32 {
    // SAFETY: `frame` is a live slice for the duration of the call, so
    // `(ptr, len)` names exactly its valid range; the host copies the
    // bytes out synchronously and retains no pointer past return (the
    // same contract as `host_console_write`).
    unsafe { rustos_host_present_framebuffer(frame.as_ptr(), frame.len(), width, height, stride) }
}
