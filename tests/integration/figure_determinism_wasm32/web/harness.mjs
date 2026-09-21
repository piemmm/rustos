// Harness for the wasm32 leg of the figure engine's determinism
// vertical, run by `cargo xtask test --wasm`.
//
// No browser and no puppeteer: this vertical's subject is arithmetic, so
// Node's own WebAssembly engine is a complete host for it. The module
// exports the digest it produced and the constant it must equal; the
// harness compares them and reports PASS or FAIL.
//
// Usage: node harness.mjs --wasm <path to the .wasm>

import { readFile } from 'node:fs/promises';

const flag = process.argv.indexOf('--wasm');
if (flag < 0 || flag + 1 >= process.argv.length) {
  console.error('FAIL: usage: harness.mjs --wasm <module.wasm>');
  process.exit(2);
}

const bytes = await readFile(process.argv[flag + 1]);
const { instance } = await WebAssembly.instantiate(bytes, {});
const { figure_digest, reference_digest } = instance.exports;

if (typeof figure_digest !== 'function' || typeof reference_digest !== 'function') {
  console.error('FAIL: the module does not export the determinism entry points');
  process.exit(2);
}

let produced;
let expected;
try {
  produced = figure_digest();
  expected = reference_digest();
} catch (error) {
  // A trap is how the module reports a panic, and nothing in the
  // generator may panic.
  console.error(`FAIL: the figure engine trapped: ${error}`);
  process.exit(1);
}

const hex = (value) => `0x${BigInt.asUintN(64, value).toString(16).padStart(16, '0')}`;

if (produced === 0n) {
  console.error('FAIL: the grid did not place');
  process.exit(1);
}
if (produced !== expected) {
  console.error(`FAIL: digest ${hex(produced)}, expected ${hex(expected)}`);
  process.exit(1);
}

console.log(`PASS: wasm32 digest ${hex(produced)} matches the reference`);
