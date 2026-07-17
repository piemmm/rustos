// TAIRiX wasm32 secondary-CPU worker bootstrap (`plans/WIRING.md` Stage
// W8).
//
// A bare-metal port's secondary core starts at an assembly trampoline;
// the wasm32 port's secondary CPU is a Web Worker that starts here. The
// main thread (`tairix.js` `boot`/`startWorker`) constructs this module
// worker and posts it the compiled module bytes, this worker's logical
// CPU index, and the `MessagePort` joining it to the main thread.
//
// All the instantiation logic is shared with the main thread in
// `tairix.js`; this file is only the worker-side entry point, kept
// dependency-free like the rest of the host glue (`AGENTS.md` §2.12).

import { runWorker } from "./tairix.js";

runWorker();
