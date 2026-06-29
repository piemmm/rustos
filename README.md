# RustOS (not it's final name) - An OS experiment.

A security-first, multi-user, multi-core operating system written in Rust,
targeting bare-metal x86_64, AArch64, RISC-V 64, and the browser via
`wasm32-unknown-unknown`.

This file is intentionally brief. Authoritative documents live alongside the
code:

- [`AGENTS.md`](./AGENTS.md) — binding engineering charter.
- [`PLAN.md`](./PLAN.md) — staged delivery plan.
- [`docs/`](./docs) — long-form architecture, security, and platform book
  (built with mdBook).

## Status
**Work in progress.** - There is a long way to go before this project is ready
for prime time, if it ever will be. <span style="color:red">**Do not expect anything to work yet, Do *not* use it.**</span>

## Feature / architecture support

Per-architecture state of features whose support varies by target. Legend:
`✓` implemented · `◐` in progress · `▢` planned · `—` not applicable.
Architecture-neutral subsystems (kernel core, scheduler, IPC, capabilities,
filesystems, userland, desktop) are tracked in [`PLAN.md`](./PLAN.md) and,
for filesystems, the block below.

| Feature | x86_64 | aarch64 | riscv64 | wasm32 |
| --- | :-: | :-: | :-: | :-: |
| Boot + early console | ✓ | ✓ | ✓ | ✓ |
| Hardware discovery | ✓ ACPI | ✓ FDT | ✓ FDT | ✓ host |
| MMU / paging | ✓ | ✓ | ✓ | — |
| Context switch | ✓ | ✓ | ✓ | — |
| Interrupts + timer | ✓ | ✓ | ✓ | ✓ |
| SMP bring-up | ✓ | ✓ | ✓ | ✓ |
| Heterogeneous CPUs (big.LITTLE / hybrid) | ✓ CPUID | ✓ FDT | ▢ | — |
| Cache-aware scheduling (LLC-aware) | ▢ | ▢ | ▢ | — |
| Cross-CPU TLB shootdown | ✓ | ✓ | ✓ | — |
| Syscall entry | ✓ | ✓ | ✓ | ✓ |
| User-mode execution (ring 3 / EL0 / U-mode) | ✓ | ✓ | ✓ | — |
| C-callable ABI (`abi-v1`, non-Rust) | ✓ | ✓ | ✓ | — |
| Side-channel mitigation | ✓ | ✓ | ✓ | ✓ |
| Memory tagging (software UAF floor) | ✓ | ✓ | ✓ | ✓ |
| Framebuffer / display | ✓ | ✓ | ▢ | ✓ |
| Block storage | ✓ virtio | ✓ virtio + eMMC | ✓ virtio | — |
| Networking | ◐ virtio | ◐ virtio | ◐ virtio | — |
| Input devices | ✓ ps2 + USB | ✓ virtio + USB | ✓ virtio | ✓ host |
| Production kernel binary | ✓ | ✓ | ▢ | ▢ |
| Bootable image | ▢ iso | ✓ rpi.img | ▢ | ▢ |

Networking is the virtio-net link-layer driver plus a test ARP/ICMP-echo
responder only; there is no IP stack (TCP/UDP/IPv4 routing) yet, hence `◐`.

Filesystems are architecture-neutral — one crate runs on every bare-metal
target (wasm32 has no block device), so per-target ticks add nothing:

| Filesystem | State |
| --- | :-: |
| ext4 | ✓ read/write |
| FAT32 | ✓ read/write |
| RustFS (native) | ✓ |

## Security & attack-vector prevention

The attack classes RustOS forecloses, and where each defence stands per
target. The structural defences (capability authority, process isolation,
no ambient root, signed code) are designed in from the kernel up; the
hardening defences below are the [`AGENTS.md`](./AGENTS.md) §4/§5/§19
controls, tracked against the §19 burn-down in [`PLAN.md`](./PLAN.md). Same
legend as above: `✓` implemented · `◐` in progress · `▢` planned ·
`—` not applicable. Architecture-neutral rows are `✓` on every target by
design; rows that depend on the MMU or on backing storage are `—` on
`wasm32` (it runs in the browser's sandbox with no page tables or swap).

| Defence (`AGENTS.md` §) | Attack vector closed | x86_64 | aarch64 | riscv64 | wasm32 |
| --- | --- | :-: | :-: | :-: | :-: |
| Capability authority, no ambient root (§4, §5.2) | Privilege escalation, confused-deputy, setuid abuse | ✓ | ✓ | ✓ | ✓ |
| Hardware process isolation (§4) | Cross-process memory disclosure / tampering | ✓ MMU | ✓ MMU | ✓ MMU | ✓ host |
| Per-call capability + input checks, fail-closed (§5.4) | Unauthorised syscall/IPC/driver access | ✓ | ✓ | ✓ | ✓ |
| W^X + position-independent executables (§19.2) | Code injection, writable-executable memory | ✓ | ✓ | ✓ | ✓ |
| Load-time CFI tag vs syscall-hash (§19.2) | Control-flow hijacking across ABI/IPC | ✓ | ✓ | ✓ | ✓ |
| Software memory tagging (§19.10) | Use-after-free (software floor) | ✓ | ✓ | ✓ | ✓ |
| Zero-on-free of secrets (§4) | Secret recovery from reused memory | ✓ | ✓ | ✓ | ✓ |
| Speculation barriers on syscall / context switch (§19.1) | Spectre / MDS / L1TF / MMIO stale data | ✓ | ✓ | ✓ | ✓ host |
| Stack + slab guard pages, hardware fault (§4) | Stack/heap overrun into adjacent memory | ✓ | ✓ | ✓ | — |
| Encrypted root + encrypted swap, no plaintext mode (§4, §11) | Secret/data recovery at rest | ✓ | ✓ | ✓ | — |
| Capability-gated, bounded DMA/MMIO (§4, §18.1) | Malicious-device DMA, unbounded device memory | ✓ | ✓ | ✓ | — |
| Continuous fuzzing of parsers/ABI/IPC/syscalls (§19.6) | Input-handling memory-safety bugs | ✓ | ✓ | ✓ | ✓ |
| Hash-chained tamper-evident audit log (§19.4) | Log tampering, forensic evasion | ◐ | ◐ | ◐ | ◐ |
| Signed driver / app manifests (§9, §16.5) | Unsigned / malicious code execution | ◐ | ◐ | ◐ | ◐ |
| Supply-chain pinning: SBOM, source-hash, advisory SLA (§19.3) | Dependency compromise (xz-utils class) | ◐ | ◐ | ◐ | ◐ |
| Stack canaries / shadow stack (§19.2) | Return-address / saved-state overwrite | ◐ | ◐ | ◐ | ◐ |
| KPTI / kernel-user address-space isolation (§19.1) | Meltdown-class kernel-memory disclosure | ◐ | ◐ | ◐ | — |
| Minimum-capability parser sandboxes (§19.5) | Untrusted-input parser compromise (font/image/net) | ▢ | ▢ | ▢ | ▢ |
| Hardware memory tagging — MTE / ADI (§19.10) | Use-after-free (hardware-enforced) | — | ▢ | ▢ | — |

`◐` rows have their architecture-neutral core landed with the remaining
stage-blocked work (signed log anchors, the driver-signing trust anchor,
reproducible builds, shadow stacks, KPTI page-table isolation) tracked in
the [`PLAN.md`](./PLAN.md) §19 burn-down — not deferred by choice. The
explicit non-goals (phishing, physical/cold-boot attacks, compromise of an
admin capability holder, compiler bugs) are listed in `AGENTS.md` §19.9.

## Building

```sh
cargo xtask ci          # Full pipeline a PR must pass
cargo xtask test        # Host-side unit and integration tests
cargo xtask docs-check  # rustdoc + mdBook (with link checking)
cargo xtask --help      # All subcommands
```

The pinned nightly toolchain in [`rust-toolchain.toml`](./rust-toolchain.toml)
is installed automatically when `rustup` is present. External tools used by
`cargo xtask ci` are:

```sh
cargo install --locked cargo-deny mdbook
```

## Licence

Licensed under the [GNU General Public License v2.0 or later](./LICENSE)
(GPL-2.0-or-later), with an additional syscall / ABI exception
(`RustOS-syscall-note`) that keeps user-space programs which merely use the
kernel's system calls or its published syscall / ABI interface definitions
from being treated as derived works. See [`LICENSE`](./LICENSE) for the full
text.

RustOS is an independent, open-source hobby project. It is not affiliated with, endorsed by, or supported by the Rust Project or the Rust Foundation.
