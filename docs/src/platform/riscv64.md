# riscv64

RustOS targets `riscv64gc-unknown-none-elf` as a Tier-1 platform. The
kernel port itself is staged work; what exists today is the host-side
**QEMU runner** support that the Stage 4.D virtio-MMIO integration tests
build on, plus the kernel-side `SiFive` Test finisher
(`kernel/arch/riscv64::qemu_exit`) those tests use to report their
result. This page documents that runner surface — the on-board boot
model, the result protocol, and the argv contract — so the kernel-side
test bin can rely on a stable harness.

## Board model: `virt`

The runner targets QEMU's generic `virt` board (`qemu-system-riscv64 -M
virt`). Unlike x86_64 there is no firmware ISO step: `-bios default`
loads the OpenSBI firmware bundled with QEMU, which jumps to the ELF
supplied via `-kernel`. The kernel ELF is therefore the bootable
artifact directly — `Runner::run` passes `spec.kernel` straight through
to the riscv64 argv builder.

The `virt` board carries the devices the Stage 4.D drivers exercise: a
SiFive Test device, eight virtio-mmio transports, and a generic PCIe
host bridge. A backing image attached with `Spec::with_virtio_blk`
surfaces as a `virtio-blk-device` on one of the virtio-mmio transports —
the riscv64 analogue of the x86_64 `virtio-blk-pci` function, driven by
`drivers/bus/virtio::MmioTransport`. A network interface attached with
`Spec::with_virtio_net` / `with_virtio_net_pcap(path)` surfaces the same
way as a `virtio-net-device` on a virtio-mmio transport, behind QEMU's
user-mode (SLIRP) backend (`-netdev user`); the optional `pcap` path
attaches a `filter-dump` so the host harness can verify the ARP/ICMP
exchange after the run.

## Result protocol: SiFive Test device

x86_64 reports a test result through the `isa-debug-exit` device as a
*non-zero* QEMU process status (`(0x10 << 1) | 1`). riscv64 has no such
device; the `virt` board exposes a SiFive Test (`sifive_test`) finisher
at MMIO base `0x10_0000` instead. The kernel writes a 32-bit word there:

- `FINISHER_PASS` (`0x5555`) makes QEMU exit with process status `0`.
  The runner treats this — and only this — as success.
- `FINISHER_FAIL` (`0x3333`) in the low half, with an exit code in the
  high half (`(code << 16) | 0x3333`), makes QEMU exit with that `code`.
  Every non-zero status is a failure.

Because success is a *zero* status on riscv64 and a *non-zero* status on
x86_64, the exit-status decode is per-architecture:
`Arch::outcome_from_status` dispatches to `riscv64::outcome_from_status`
(zero ⇒ `Pass`) or `Outcome::from_qemu_status` (x86_64 convention). The
finisher constants live beside the argv builder in
`tools/qemu/src/riscv64.rs` and are pinned by a unit test; the
kernel-side `kernel/arch/riscv64::qemu_exit` mirrors the same values
(`SIFIVE_TEST_BASE`, `FINISHER_PASS`, `FINISHER_FAIL`) with its own
tie-down test, so the two sides cannot drift. The kernel writes the
finisher word through `qemu_exit::exit_success` / `exit_failure(code)`;
the failure word is built by the pure `qemu_exit::fail_word(code)`
(`(code << 16) | FINISHER_FAIL`).

## Per-arch runner module

| Surface | Module |
|---|---|
| `Outcome`, `Arch`, `Spec`, `Runner`, per-arch exit decode dispatch | `tools/qemu/src/lib.rs` (architecture-neutral) |
| `DEFAULT_RAM_MIB`, `QEMU_BINARY`, `MACHINE`, `SIFIVE_TEST_BASE`, `FINISHER_PASS/FAIL`, `outcome_from_status`, `virt` argv assembly | `tools/qemu/src/riscv64.rs` |

The argv contract — `-M virt`, `-no-reboot`, `-display none`, `-serial
stdio`, `-m {DEFAULT_RAM_MIB}M`, `-smp {spec.cpus}`, `-bios default`,
`-kernel {elf}`, and one `-drive if=none,format=raw,id=blkN,file=…` +
`-device virtio-blk-device,drive=blkN` pair per backing image, plus one
`-netdev user,id=netN` + `-device virtio-net-device,netdev=netN` pair
(and an optional `-object filter-dump`) per network interface — is
asserted by host unit tests in `tools/qemu/src/riscv64.rs::tests`. They
use the same pure `build_argv` helper pattern as the x86_64 backend, so
they run without spawning QEMU. The `Spec::for_riscv64_kernel`,
`with_cpus`, `with_timeout`, `with_virtio_blk`, `with_virtio_net`,
`with_virtio_net_pcap`, and `Runner::run` entry points are shared with
x86_64; only the per-arch backend differs (`AGENTS.md` §2.4 — no
interface creep).

## Manual debugging

The `rustos-qemu-run` wrapper is x86_64-only today; riscv64 runs go
through `Runner::run` (or `cargo xtask test --qemu` once a riscv64 test
crate is enrolled). Set `RUSTOS_QEMU_DEBUG=1` to print the exact QEMU
invocation the runner constructs.
