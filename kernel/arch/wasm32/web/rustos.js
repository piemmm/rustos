// RustOS wasm32 host loader (`AGENTS.md` §3 / Stage 3d).
//
// This is the JavaScript counterpart of the bare-metal ports' firmware
// hand-off: it instantiates a RustOS wasm32 module, supplies the `env`
// host imports the port declares in `kernel/arch/wasm32/src/bindings.rs`,
// and starts the cooperative scheduler. It is hand-written and
// dependency-free, mirroring the no-`wasm-bindgen` policy of the Rust
// side (`AGENTS.md` §2.12).
//
// The host surface, in lock-step with `bindings.rs`:
//
//   rustos_host_now_ms()        -> performance.now()
//   rustos_host_current_worker() -> this context's worker index
//   rustos_host_post_ipi(worker) -> MessageChannel post (cooperative IPI)
//   rustos_host_request_frame()  -> requestAnimationFrame(on_frame)
//   rustos_host_console_write(ptr, len) -> decode UTF-8 from the module's
//                                          linear memory and emit a line
//   rustos_host_logical_processors() -> navigator.hardwareConcurrency (>= 1)
//   rustos_host_has_display()        -> 1 if a display surface is present
//
// The module exports the entry trampoline and the host callbacks
// (`kernel/arch/wasm32/src/entry.rs`):
//
//   rustos_arch_wasm32_main()       boot entry (called once)
//   rustos_arch_wasm32_on_frame()   per requestAnimationFrame tick
//   rustos_arch_wasm32_on_message() per delivered MessageChannel message

/**
 * Instantiate a RustOS wasm32 module and run its boot trampoline.
 *
 * @param {BufferSource} wasmBytes  the compiled `.wasm` module bytes.
 * @param {object} [hooks]
 * @param {(line: string) => void} [hooks.onLine]  one complete console
 *        line (newline-terminated chunks are reassembled for you).
 * @param {() => number} [hooks.now]   monotonic clock in ms.
 * @param {number} [hooks.worker]      this context's worker index.
 * @param {(cb: () => void) => void} [hooks.requestFrame]  frame scheduler.
 * @param {(worker: number) => void} [hooks.postIpi]  IPI delivery hook.
 * @param {number} [hooks.logicalProcessors]  CPU count (defaults to
 *        navigator.hardwareConcurrency, clamped to >= 1).
 * @param {boolean} [hooks.hasDisplay]  whether a display surface exists.
 * @returns {Promise<WebAssembly.Instance>}
 */
export async function boot(wasmBytes, hooks = {}) {
  const onLine = hooks.onLine ?? ((line) => console.log(line));
  const now = hooks.now ?? (() => performance.now());
  const worker = (hooks.worker ?? 0) >>> 0;
  const requestFrame =
    hooks.requestFrame ?? ((cb) => globalThis.requestAnimationFrame(cb));
  const postIpi = hooks.postIpi ?? (() => {});
  const logicalProcessors = Math.max(
    1,
    (hooks.logicalProcessors ?? globalThis.navigator?.hardwareConcurrency ?? 1) >>> 0,
  );
  const hasDisplay =
    hooks.hasDisplay ?? typeof globalThis.OffscreenCanvas !== "undefined";

  const decoder = new TextDecoder("utf-8");
  // The console import delivers a line in one or more chunks (the Rust
  // `core::fmt::Write` adapter writes the message and the trailing
  // newline separately); reassemble complete lines before forwarding.
  let pending = "";
  /** @type {WebAssembly.Instance} */
  let instance;

  const flushLines = (text) => {
    pending += text;
    let nl;
    while ((nl = pending.indexOf("\n")) >= 0) {
      onLine(pending.slice(0, nl));
      pending = pending.slice(nl + 1);
    }
  };

  const env = {
    rustos_host_now_ms: () => now(),
    rustos_host_current_worker: () => worker,
    rustos_host_post_ipi: (w) => postIpi(w >>> 0),
    rustos_host_request_frame: () => {
      requestFrame(() => instance.exports.rustos_arch_wasm32_on_frame());
    },
    rustos_host_console_write: (ptr, len) => {
      const view = new Uint8Array(instance.exports.memory.buffer, ptr, len);
      flushLines(decoder.decode(view));
    },
    rustos_host_logical_processors: () => logicalProcessors,
    rustos_host_has_display: () => (hasDisplay ? 1 : 0),
  };

  const { instance: inst } = await WebAssembly.instantiate(wasmBytes, { env });
  instance = inst;
  instance.exports.rustos_arch_wasm32_main();
  return instance;
}
