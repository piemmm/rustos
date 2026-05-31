// Browser-headless harness for the Stage 3d wasm32 boot vertical.
//
// This is the wasm32 analogue of the bare-metal `tools/qemu` runner: it
// boots the compiled kernel module in a real (headless) browser and
// decides PASS/FAIL from what the kernel prints. `cargo xtask test
// --wasm` builds the `.wasm` and launches this script.
//
// It asserts the three Stage-3 per-sub-stage deliverables (`PLAN.md`):
//   * BOOT_OK      — the wasm32 Arch HAL booted to `init`.
//   * ISOLATION_OK — the WASM-memory isolation model denied a
//                    cross-context access.
//   * >= MIN_TICKS `TICK` lines — the `requestAnimationFrame` cooperative
//                    loop drives the scheduler tick callback.
//
// Any kernel panic traps the instance and surfaces as a page error /
// HARNESS_ERROR line, failing the run loudly with no retries
// (`AGENTS.md` §7).

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import puppeteer from "puppeteer";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "../../../..");

function parseArgs(argv) {
  const args = { timeoutSecs: 30, minTicks: 20, chrome: null, wasm: null };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === "--wasm") args.wasm = argv[++i];
    else if (a === "--chrome") args.chrome = argv[++i];
    else if (a === "--timeout-secs") args.timeoutSecs = Number(argv[++i]);
    else if (a === "--min-ticks") args.minTicks = Number(argv[++i]);
    else throw new Error(`unknown argument: ${a}`);
  }
  if (!args.wasm) throw new Error("missing required --wasm <path>");
  return args;
}

const CONTENT_TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
};

// Static routes: the shared host loader, this vertical's page, and the
// compiled module. Everything else 404s — the harness serves exactly
// what the page needs, nothing more.
function buildRoutes(wasmPath) {
  return {
    "/rustos.js": {
      file: resolve(REPO_ROOT, "kernel/arch/wasm32/web/rustos.js"),
      type: CONTENT_TYPES[".js"],
    },
    "/index.html": {
      file: resolve(HERE, "index.html"),
      type: CONTENT_TYPES[".html"],
    },
    "/module.wasm": { file: resolve(wasmPath), type: CONTENT_TYPES[".wasm"] },
  };
}

async function startServer(routes) {
  const server = createServer(async (req, res) => {
    const url = (req.url || "/").split("?")[0];
    const route = routes[url];
    if (!route) {
      res.writeHead(404);
      res.end("not found");
      return;
    }
    try {
      const body = await readFile(route.file);
      res.writeHead(200, { "content-type": route.type });
      res.end(body);
    } catch (err) {
      res.writeHead(500);
      res.end(String(err));
    }
  });
  await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
  return server;
}

async function run() {
  const args = parseArgs(process.argv.slice(2));
  const routes = buildRoutes(args.wasm);
  const server = await startServer(routes);
  const { port } = server.address();

  const launchOpts = {
    headless: true,
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
  };
  const exe = args.chrome || process.env.PUPPETEER_EXECUTABLE_PATH || "/usr/bin/google-chrome";
  if (exe) launchOpts.executablePath = exe;

  const browser = await puppeteer.launch(launchOpts);
  let pass = false;
  let reason = "timeout";
  try {
    const page = await browser.newPage();
    const lines = [];
    let resolveDone;
    const done = new Promise((ok) => (resolveDone = ok));

    const check = () => {
      const ticks = lines.filter((l) => l === "TICK").length;
      if (
        lines.includes("BOOT_OK") &&
        lines.includes("ISOLATION_OK") &&
        ticks >= args.minTicks
      ) {
        pass = true;
        resolveDone();
      }
      if (lines.some((l) => l.startsWith("HARNESS_ERROR"))) {
        reason = lines.find((l) => l.startsWith("HARNESS_ERROR"));
        resolveDone();
      }
    };

    page.on("console", (msg) => {
      lines.push(msg.text());
      check();
    });
    page.on("pageerror", (err) => {
      reason = `pageerror: ${err.message}`;
      resolveDone();
    });

    await page.goto(`http://127.0.0.1:${port}/index.html?wasm=/module.wasm`);

    const timeout = new Promise((ok) =>
      setTimeout(() => ok("timeout"), args.timeoutSecs * 1000),
    );
    await Promise.race([done, timeout]);

    const ticks = lines.filter((l) => l === "TICK").length;
    console.log(
      `[wasm harness] BOOT_OK=${lines.includes("BOOT_OK")} ` +
        `ISOLATION_OK=${lines.includes("ISOLATION_OK")} ticks=${ticks}`,
    );
    if (!pass) {
      console.log(`[wasm harness] FAIL (${reason})`);
      console.log(`[wasm harness] captured lines: ${JSON.stringify(lines)}`);
    }
  } finally {
    await browser.close();
    server.close();
  }

  if (pass) {
    console.log("[wasm harness] PASS");
    process.exit(0);
  }
  process.exit(1);
}

run().catch((err) => {
  console.error(`[wasm harness] error: ${err && err.stack ? err.stack : err}`);
  process.exit(1);
});
