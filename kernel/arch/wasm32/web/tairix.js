// TAIRiX wasm32 host loader (`AGENTS.md` §3 / Stage 3d, multi-worker
// SMP added in `plans/WIRING.md` Stage W8).
//
// This is the JavaScript counterpart of the bare-metal ports' firmware
// hand-off: it instantiates a TAIRiX wasm32 module, supplies the `env`
// host imports the port declares in `kernel/arch/wasm32/src/bindings.rs`,
// and starts the cooperative scheduler. It is hand-written and
// dependency-free, mirroring the no-`wasm-bindgen` policy of the Rust
// side (`AGENTS.md` §2.12).
//
// The host surface, in lock-step with `bindings.rs`:
//
//   tairix_host_now_ms()         -> performance.now()
//   tairix_host_current_worker() -> this context's worker index
//   tairix_host_post_ipi(worker) -> MessageChannel post (cooperative IPI)
//   tairix_host_start_worker(w)  -> spawn a Web Worker as logical CPU w
//   tairix_host_request_frame()  -> requestAnimationFrame(on_frame)
//   tairix_host_console_write(ptr, len) -> decode UTF-8 from the module's
//                                          linear memory and emit a line
//   tairix_host_logical_processors() -> navigator.hardwareConcurrency (>= 1)
//   tairix_host_has_display()        -> 1 if a display surface is present
//   tairix_host_present_framebuffer(ptr, len, w, h, stride) -> paint a
//                                          canvas from the module's linear
//                                          memory and return the count of
//                                          pixels that survived the round-trip
//
// The module exports the entry trampoline and the host callbacks
// (`kernel/arch/wasm32/src/entry.rs`):
//
//   tairix_arch_wasm32_main()       boot entry (called once)
//   tairix_arch_wasm32_on_frame()   per requestAnimationFrame tick
//   tairix_arch_wasm32_on_message() per delivered MessageChannel message
//
// # Multi-worker (SMP) topology
//
// The boot context is the main thread, logical CPU 0. When the kernel
// calls `tairix_host_start_worker(n)`, the main thread spawns a real Web
// Worker that instantiates this same module as logical CPU `n`, with its
// own linear memory (the wasm32 isolation boundary). Main and each worker
// are joined by a `MessageChannel`; an inter-context IPI
// (`tairix_host_post_ipi`) is a post on that channel that re-enters the
// target's `tairix_arch_wasm32_on_message`. The main thread is the hub:
// a worker→worker IPI is routed through it. Web Workers have no
// `requestAnimationFrame`, so a worker drives its cooperative tick from
// `setTimeout` instead — the kernel side is identical (`request_frame`).

/**
 * Build the `env` import object a module instance is instantiated with.
 *
 * `ctx` carries the per-context host hooks (clock, this context's worker
 * index, frame scheduler, IPI sink, worker spawn, console, capability
 * counts) plus `getInstance()`, which the memory-touching imports use to
 * reach the live instance's exports.
 *
 * @param {object} ctx
 * @returns {WebAssembly.Imports["env"]}
 */
function makeEnv(ctx) {
  const decoder = new TextDecoder("utf-8");
  // The console import delivers a line in one or more chunks (the Rust
  // `core::fmt::Write` adapter writes the message and the trailing
  // newline separately); reassemble complete lines before forwarding.
  let pending = "";
  const flushLines = (text) => {
    pending += text;
    let nl;
    while ((nl = pending.indexOf("\n")) >= 0) {
      ctx.onLine(pending.slice(0, nl));
      pending = pending.slice(nl + 1);
    }
  };

  return {
    tairix_host_now_ms: () => ctx.now(),
    tairix_host_current_worker: () => ctx.worker >>> 0,
    tairix_host_post_ipi: (w) => ctx.postIpi(w >>> 0),
    tairix_host_start_worker: (w) => (ctx.startWorker(w >>> 0) ? 1 : 0),
    tairix_host_request_frame: () => {
      ctx.requestFrame(() =>
        ctx.getInstance().exports.tairix_arch_wasm32_on_frame(),
      );
    },
    tairix_host_console_write: (ptr, len) => {
      const view = new Uint8Array(
        ctx.getInstance().exports.memory.buffer,
        ptr,
        len,
      );
      flushLines(decoder.decode(view));
    },
    tairix_host_logical_processors: () => ctx.logicalProcessors >>> 0,
    tairix_host_has_display: () => (ctx.hasDisplay ? 1 : 0),
    tairix_host_present_framebuffer: (ptr, len, width, height, stride) => {
      const view = new Uint8Array(
        ctx.getInstance().exports.memory.buffer,
        ptr,
        len,
      );
      return (
        ctx.presentFramebuffer(view, width >>> 0, height >>> 0, stride >>> 0) >>>
        0
      );
    },
  };
}

/**
 * Instantiate a TAIRiX wasm32 module against a host context.
 *
 * Wires up `env` and resolves to the live instance. It does **not** call
 * the boot trampoline: the caller installs any message handlers first,
 * then calls `tairix_arch_wasm32_main()`.
 *
 * @param {BufferSource} wasmBytes
 * @param {object} ctx  the per-context host hooks (see {@link makeEnv}).
 * @returns {Promise<WebAssembly.Instance>}
 */
export async function instantiate(wasmBytes, ctx) {
  /** @type {WebAssembly.Instance} */
  let instance;
  ctx.getInstance = () => instance;
  const { instance: inst } = await WebAssembly.instantiate(wasmBytes, {
    env: makeEnv(ctx),
  });
  instance = inst;
  return instance;
}

/**
 * Instantiate and boot a TAIRiX wasm32 module on the main thread as
 * logical CPU 0, including the multi-worker SMP plumbing.
 *
 * @param {ArrayBuffer} wasmBytes  the compiled `.wasm` module bytes.
 * @param {object} [hooks]
 * @param {(line: string) => void} [hooks.onLine]  one complete console
 *        line (chunked writes are reassembled for you).
 * @param {() => number} [hooks.now]   monotonic clock in ms.
 * @param {(cb: () => void) => void} [hooks.requestFrame]  frame scheduler.
 * @param {number} [hooks.logicalProcessors]  CPU count (defaults to
 *        navigator.hardwareConcurrency, clamped to >= 1).
 * @param {boolean} [hooks.hasDisplay]  whether a display surface exists.
 * @param {(bytes: Uint8Array, width: number, height: number, stride: number) => number} [hooks.presentFramebuffer]
 *        paint an RGBA8888 surface onto the display and return the count
 *        of pixels that survived the canvas round-trip (defaults to a
 *        no-op returning 0 — a headless context with no canvas).
 * @param {string|URL} [hooks.workerUrl]  the worker bootstrap module URL.
 * @returns {Promise<WebAssembly.Instance>}
 */
export async function boot(wasmBytes, hooks = {}) {
  const onLine = hooks.onLine ?? ((line) => console.log(line));
  const now = hooks.now ?? (() => performance.now());
  const requestFrame =
    hooks.requestFrame ?? ((cb) => globalThis.requestAnimationFrame(cb));
  const logicalProcessors = Math.max(
    1,
    (hooks.logicalProcessors ??
      globalThis.navigator?.hardwareConcurrency ??
      1) >>> 0,
  );
  const hasDisplay =
    hooks.hasDisplay ?? typeof globalThis.OffscreenCanvas !== "undefined";
  const presentFramebuffer = hooks.presentFramebuffer ?? (() => 0);
  const workerUrl = hooks.workerUrl ?? new URL("./worker.js", import.meta.url);

  // Live secondary CPUs: logical index -> { worker, port (the main end of
  // its MessageChannel) }.
  const workers = new Map();

  /** @type {object} */
  let ctx;

  // Deliver a cooperative reschedule to logical CPU `target`. Target 0 is
  // the main thread (a self-reschedule), delivered on a microtask so it
  // never re-enters synchronously; any other target is a worker reached
  // through its MessageChannel. An unknown target is dropped (best-effort
  // delivery, mirroring a lost hardware IPI).
  const deliverIpi = (target) => {
    if (target === 0) {
      queueMicrotask(() =>
        ctx.getInstance().exports.tairix_arch_wasm32_on_message(),
      );
      return;
    }
    const entry = workers.get(target);
    if (entry) entry.port.postMessage({ t: "ipi" });
  };

  // A message arriving from worker `from`: either a console line to
  // surface, or an IPI to route (the main thread is the hub).
  const handleFromWorker = (from, data) => {
    if (!data) return;
    if (data.t === "log") onLine(data.line);
    else if (data.t === "ipi") deliverIpi(data.target >>> 0);
  };

  // Spawn logical CPU `index` as a real Web Worker running this module.
  // Returns false (refused) for a duplicate index or if the host cannot
  // construct a worker.
  const startWorker = (index) => {
    if (workers.has(index)) return false;
    const channel = new MessageChannel();
    let worker;
    try {
      worker = new Worker(workerUrl, { type: "module" });
    } catch (err) {
      onLine(`HARNESS_ERROR worker ${index} spawn: ${err?.message ?? err}`);
      return false;
    }
    worker.onerror = (err) =>
      onLine(`HARNESS_ERROR worker ${index}: ${err?.message ?? err}`);
    channel.port1.onmessage = (ev) => handleFromWorker(index, ev.data);
    workers.set(index, { worker, port: channel.port1 });
    // Copy the module bytes so transferring them to the worker cannot
    // detach the buffer the main instance and later workers still need.
    const bytesCopy = wasmBytes.slice(0);
    worker.postMessage(
      { kind: "boot", wasmBytes: bytesCopy, index, port: channel.port2 },
      [bytesCopy, channel.port2],
    );
    return true;
  };

  ctx = {
    now,
    worker: 0,
    requestFrame,
    postIpi: deliverIpi,
    startWorker,
    logicalProcessors,
    hasDisplay,
    presentFramebuffer,
    onLine,
  };

  const instance = await instantiate(wasmBytes, ctx);
  instance.exports.tairix_arch_wasm32_main();
  return instance;
}

/**
 * Bootstrap entry for a spawned Web Worker (a secondary logical CPU).
 *
 * `kernel/arch/wasm32/web/worker.js` calls this in the worker global
 * scope. It waits for the main thread's boot message — the module bytes,
 * this worker's logical CPU index, and the `MessagePort` joining it to
 * the main thread — instantiates the module as that CPU, wires the port
 * to `tairix_arch_wasm32_on_message`, and runs the boot trampoline.
 */
export function runWorker() {
  globalThis.onmessage = async (event) => {
    const data = event.data;
    if (!data || data.kind !== "boot") return;
    const { wasmBytes, index, port } = data;

    const ctx = {
      now: () => performance.now(),
      worker: index >>> 0,
      // A dedicated worker has no requestAnimationFrame; setTimeout is the
      // cooperative-yield primitive. The kernel side is unchanged.
      requestFrame: (cb) => setTimeout(cb, 16),
      // Route every IPI through the main-thread hub.
      postIpi: (target) => port.postMessage({ t: "ipi", target: target >>> 0 }),
      // Nested-worker spawn is not used; report refusal honestly.
      startWorker: () => false,
      logicalProcessors: 1,
      hasDisplay: false,
      // A dedicated worker drives no display surface.
      presentFramebuffer: () => 0,
      onLine: (line) => port.postMessage({ t: "log", line }),
    };

    const instance = await instantiate(wasmBytes, ctx);
    port.onmessage = (ev) => {
      if (ev.data && ev.data.t === "ipi")
        instance.exports.tairix_arch_wasm32_on_message();
    };
    port.start?.();
    instance.exports.tairix_arch_wasm32_main();
  };
}
