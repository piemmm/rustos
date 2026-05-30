# Next session — Stage 4 remaining first-drivers (Stage 4.D is done)

## Where we are

**Stage 4.D is complete.** Its final item — Item 6, the acceptance gate
— landed this session:

- The gate was finished on a host carrying the two tools the prior
  session lacked: `mdbook` (v0.5.3) + `mdbook-linkcheck` and `cargo-deny`
  (0.19.7). (Note: they live in `~/.cargo/bin`, which is **not** on the
  default non-interactive `PATH` — `export PATH="$HOME/.cargo/bin:$PATH"`
  before invoking the gate.)
- Two real defects the now-runnable steps surfaced were fixed:
  1. **`deny.toml` licence policy.** All workspace crates declare the
     canonical SPDX `license = "GPL-3.0-only"`, but `licenses.allow`
     listed only the deprecated `GPL-3.0`, so `cargo deny check` failed
     (`licenses FAILED`). The allow entry is now `GPL-3.0-only`.
  2. **`cargo xtask coverage` tool probe.** It ran `cargo-llvm-cov
     --version` directly; `cargo-llvm-cov` is a cargo subcommand whose
     binary rejects a bare `--version`, so the probe always reported the
     tool missing. `tools/xtask/src/commands.rs` now probes cargo
     subcommands via `cargo <sub> --version`
     (`cargo_subcommand_available`), used by both `run_coverage` and
     `run_deny`; a fail-closed unit test guards it.
- Verified green on this host: `cargo xtask ci`, `cargo xtask docs-check`,
  `cargo deny check`, `cargo xtask test --qemu` (all 11 verticals),
  `cargo xtask coverage` (workspace TOTAL 93.25% region). §7 high-bar
  crates confirmed via targeted `cargo llvm-cov --summary-only`:
  `kernel/sec` ≥97%, `lib/caps` ≥98%, `lib/crypto` ≥97.67%, and
  `kernel/mem` + `kernel/ipc` + `kernel/irq` combined 95.18% region /
  95.38% line. See the Item 6 *complete* entry in `PLAN.md`.

Earlier Stage 4.D landings (Items 0–5, the virtio-PCI/MMIO QEMU
verticals, the arch-neutral `kernel/virtio` crate, the riscv64 port) are
all complete — see `PLAN.md` Stage 4.D.

## What needs doing — remaining Stage 4 first drivers

Stage 4's deliverable list (`PLAN.md` "Stage 4 — Driver Framework and
First Drivers", **Status: in progress**) still has these per-class first
drivers outstanding:

- `drivers/display/vesa` (x86_64 BIOS).
- `drivers/display/framebuffer` (aarch64 Pi, riscv64 virt, wasm32 canvas).
- `drivers/input/ps2` (x86_64).

Each must, per `AGENTS.md` §8: implement the relevant
`lib/abi/src/driver/<class>.rs` trait(s), expose only `pub fn
register(...)`, ship a `README.md` (supported HW, caps, limits), carry
mock-host unit tests, and — where the hardware is emulable — at least one
QEMU integration test (load → use → unload → reload). Pick one driver,
implement it fully (code + tests + rustdoc + `docs/src/drivers/` page),
and land it before starting the next. Packed virtqueues (virtio 1.1
§2.7) remain a Stage 5 follow-up.

## Verification commands

```
export PATH="$HOME/.cargo/bin:$PATH"   # mdbook / cargo-deny / cargo-llvm-cov live here

# Full gate (what a PR must pass):
cargo xtask ci
cargo xtask test --qemu

# Coverage (workspace TOTAL + targeted high-bar confirmation):
cargo xtask coverage
cargo llvm-cov --summary-only -p rustos-kernel-mem -p rustos-kernel-ipc -p rustos-kernel-irq
```
