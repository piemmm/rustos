# Next session — Stage 4 remaining QEMU verticals (ps2 vertical is done)

## Where we are

All per-class Stage 4 **first drivers** are implemented with mock-host
unit tests, rustdoc, and `docs/` pages (`PLAN.md` Stage 4 deliverable
list): `drivers/display/{vesa,framebuffer}`, `drivers/input/ps2`,
`drivers/bus/{pci,mmio,virtio}`, `drivers/storage/virtio_blk`,
`drivers/network/virtio_net`. The userland driver host
(`userland/system/drvhost`) loads/unloads/reloads signed `.rxe` modules
with capability enforcement.

**This session landed the first display/input-class QEMU integration
vertical**, closing the `load → use device → unload → reload` gap for an
input driver:

- `kernel/arch/x86_64/src/pio.rs` gained `X86PortIo8` + `x86_port_io8()`
  — the byte-wide sibling of the PCI bus driver's 32-bit `X86PortIo` and
  the only in-tree implementor of the `rustos_abi::PortIo8` seam
  (`in al, dx` / `out dx, al` behind the safe trait, each `// SAFETY:`-
  documented; unit-tested). This is the boot hand-off prerequisite the
  ps2 driver had been waiting on.
- `tests/integration/ps2_input_qemu_x86_64`
  (`rustos-test-ps2-qemu-x86-64`) boots the production kernel, loads the
  signed ps2 `.rxe` through `rustos_drvhost::Host`, then mints
  `X86PortIo8` and drives a real `Ps2Keyboard` through load → use →
  unload → reload. "Use" is deterministic without a keypress via the
  i8042 `0xD2` ("write keyboard output buffer") command: the test
  injects a scancode through the same `PortIo8` backend the driver reads
  through and confirms the decoded press, then the matching release
  after reload. Enrolled in `tools/xtask/src/commands/qemu_tests.rs`.
- Docs refreshed: `docs/src/drivers/input.md`,
  `drivers/input/ps2/README.md`, `PLAN.md` Stage 4 status.

Verified green on this host: `cargo xtask ci` (fmt → clippy → test
`--qemu` → docs-check → deny → abi-check) plus `deps-check` / `cfg-check`,
and the ps2 vertical PASSes standalone via `rustos-qemu-run`.

(Note: `mdbook`/`mdbook-linkcheck`, `cargo-deny`, `cargo-llvm-cov` live
in `~/.cargo/bin`, which is **not** on the default non-interactive
`PATH` — `export PATH="$HOME/.cargo/bin:$PATH"` before invoking the
gate.)

## What needs doing — remaining Stage 4 QEMU verticals

Per `AGENTS.md` §8, every emulable driver needs at least one QEMU
integration test (load → use → unload → reload). Still outstanding for
the display class:

- `drivers/display/framebuffer` — needs a framebuffer boot hand-off: the
  kernel publishing an already-parsed geometry record + a `MmioMapper`
  over the linear framebuffer the driver can `map_window` through. Most
  tractable on a target with a simple linear framebuffer (e.g. aarch64
  Pi / riscv64 virt ramfb, or x86_64 via a bochs/ramfb device).
- `drivers/display/vesa` — needs the bootloader-captured VBE
  `ModeInfoBlock` published as a boot capability plus a `MmioMapper`
  over `PhysBasePtr` (the same framebuffer hand-off shape as above).

Pick one, implement its boot hand-off + QEMU vertical fully (code +
tests + rustdoc + `docs/` refresh), and land it before the next. Model
the vertical on `tests/integration/ps2_input_qemu_x86_64` (boot the
production kernel via the audit-sink hook, load the signed `.rxe`
through `rustos_drvhost::Host`, exercise the device, then `qemu_exit`).
Packed virtqueues (virtio 1.1 §2.7) remain a Stage 5 follow-up.

## Verification commands

```
export PATH="$HOME/.cargo/bin:$PATH"   # mdbook / cargo-deny / cargo-llvm-cov live here

# Full gate (what a PR must pass):
cargo xtask ci
cargo xtask test --qemu

# Run a single QEMU vertical standalone (fast iteration):
cargo build --locked -p rustos-test-ps2-qemu-x86-64 --target x86_64-unknown-none
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/x86_64-unknown-none/debug/rustos-test-ps2-qemu-x86-64 \
    --cpus 1 --timeout-secs 60      # runner exit 0 == PASS (serial dumped only on failure)
```
