# riscv64

RustOS targets `riscv64gc-unknown-none-elf` as a Tier-1 platform. Two
halves exist today: the kernel-side **boot pipeline** that brings the
QEMU `virt` board up to `AuditEvent::BootCompleted`, and the host-side
**QEMU runner** that launches it (and the Stage 4.D virtio-MMIO
integration tests that build on the same harness). This page documents
both — the boot pipeline, the on-board boot model, the result protocol,
and the argv contract.

## Kernel boot pipeline

`kernel/arch/riscv64` owns the riscv64 boot path (Stage 4.D Item 4). It
boots to `AuditEvent::BootCompleted` and is exercised by the
`tests/integration/kernel_arch_boot_riscv64` QEMU test — the riscv64
analogue of the x86_64 `kernel_arch_boot` bin.

Boot sequence:

1. **Entry (`boot.s` → `entry.rs`).** OpenSBI enters the ELF in S-mode
   with paging off (`satp = 0`, bare addressing), `a0 = hartid`, and
   `a1 =` the flattened device tree pointer. The `_start` trampoline
   sets up the boot stack, zeroes `.bss`, and tail-calls
   `rustos_arch_riscv64_main(hartid, dtb)`, which forwards to the
   binary-supplied `kernel_main`.
2. **Device-tree parse (`fdt.rs`).** A minimal, bounds-checked FDT
   reader extracts the first `/memory` node's `reg` (base/size) and the
   `/cpus` `timebase-frequency`. It is host-tested against a hand-built
   DTB fixture.
3. **Boot pipeline (`boot.rs`).** Builds a `BootMemoryMap` reserving
   `[ram_base, __kernel_end)` (firmware + kernel image + boot heap) and
   marking `[__kernel_end, ram_end)` usable, constructs `RiscvArch`
   (`kernel_arch.rs`, the `kernel_core::KernelArch` impl whose monotonic
   clock reads the `time` CSR via `rdtime`), assembles a
   `kernel_core::BootInfo`, and hands it to `kernel_core::kernel_main`.
4. **Console (`sbi.rs`, `serial.rs`).** The boot log and audit records
   are written through the SBI legacy `console_putchar`, which OpenSBI
   routes to the same UART `-serial stdio` captures.

No Sv39 paging is required to reach `BootCompleted`: the board enters
S-mode with paging off and the init pipeline never faults. The boot
heap is a 64 MiB `.heap` (NOLOAD) section the linker places *after*
`__kernel_end`, so the trampoline does not zero it and the usable
physical-memory map excludes it.

> The 64 MiB boot bump allocator itself lives in the shared
> `lib/bumpalloc` crate (`rustos-bumpalloc`), registered as the test
> binary's `#[global_allocator]` — the same allocator the x86_64 boot
> bins use, defined once (`AGENTS.md` §2.2, §6).

Sv39 paging and SMP bring-up are the remaining riscv64 deliverables
(`PLAN.md` Stage 4.D Item 4); they are not needed for the
boot-to-`BootCompleted` slice. The ring-0 DTB virtio-mmio walk and the
full device bring-up now land in the virtio-MMIO QEMU verticals (below).

The kernel-side `SiFive` Test finisher (`kernel/arch/riscv64::qemu_exit`)
is what the test bin uses to report its result.

## External-interrupt controller (PLIC) + S-mode trap glue

`kernel/arch/riscv64::plic` and `kernel/arch/riscv64::trap` land the
external-IRQ foundation the virtio-mmio verticals build on. They are
implemented and host-tested; the boot pipeline itself runs with
interrupts disabled (it neither calls `trap::init_traps` nor builds a
`PlicController`). The live consumer is the virtio-MMIO QEMU verticals
(below), which `arm` the device source, install the trap dispatch, and
`init_traps`.

- **PLIC.** `plic::PlicController` wraps a `Plic<M>` register driver
  over the `PlicMmio` access seam (`VolatilePlicMmio` on the
  freestanding target). It targets the boot hart's S-mode context
  (`s_mode_context(hartid) = 2 * hartid + 1` on the `virt` layout),
  `arm`s a source (enable bit + zero threshold + delivering priority),
  and `claim`/`complete`s through the per-context claim register.
- **Mask-before-wake.** The `IrqController::mask` the kernel-neutral
  `IrqTable::fire` calls masks a source by writing its PLIC priority
  register to zero (a single lock-free 32-bit store) followed by a
  `SeqCst` fence — the riscv64 analogue of the x86_64 IO-APIC
  redirection-entry mask. See `docs/src/security/irq.md`.
- **S-mode trap vector.** `trap::init_traps` installs
  `rustos_riscv64_trap_vector` (`trap.s`) into `stvec` (direct mode)
  and enables `sie.SEIE` + `sstatus.SIE`. The vector saves
  caller-saved registers, calls the Rust handler, and `sret`s; the
  handler fails closed (parks) on a synchronous exception and forwards
  a supervisor external interrupt to a one-shot dispatch callback that
  performs the PLIC claim → `IrqTable::fire` → complete handshake.

## Boot-state publication

`kernel/arch/riscv64::publish` exposes the boot-state a driver-bring-up
observer needs as set-once slots, the riscv64 analogue of the
`rustos-kernel` bin crate's `arch_wrapper` slots on x86_64 (riscv64 owns
its boot pipeline in the arch crate, so the hooks live there too):

- `publish_memory_map` / `published_memory_map` — a `'static` clone of
  the firmware `BootMemoryMap`, published by `boot::try_boot` before the
  map is moved into the `kernel_core` hand-off, so a vertical can carve a
  per-device DMA pool from high RAM without re-borrowing the kernel state.
- `publish_dtb` / `published_dtb` — the flattened-device-tree pointer
  (`a1`), so a vertical can walk the `virtio_mmio` slots, the PLIC base,
  and each device's `interrupts` cell when it builds the MMIO transport
  and the external-IRQ path.

Both slots are one-shot (`AGENTS.md` §2.1) and the accessors expose no
writable surface (`AGENTS.md` §2.4). Unlike x86_64 there is no published
`IrqTable`: the boot-to-`BootCompleted` slice runs with interrupts
disabled and hands the kernel `IrqRouting::unsupported`, so a vertical
builds its own `PlicController` + `IrqTable` over the DTB-discovered PLIC
base rather than reusing a `max_line == 0` kernel-core table.

## virtio-MMIO QEMU verticals

`tests/integration/virtio_blk_mmio_riscv64` and
`virtio_net_mmio_riscv64` are the MMIO analogues of the x86_64
`virtio_blk_pci_x86_64` / `virtio_net_pci_x86_64` verticals: they boot
the production riscv64 pipeline and, on `AuditEvent::BootCompleted`,
drive a real virtio device over the `virt` board's virtio-mmio bus
end-to-end. The device-agnostic lifecycle and the per-device tails are
shared with the x86_64 verticals through the
`tests/integration/virtio_qemu_support` crate (`AGENTS.md` §2.2); only
the arch-specific bring-up differs (`imp_mmio` vs. `imp_pci`).

The riscv64 bring-up (`imp_mmio`):

1. Reads `published_dtb` / `published_memory_map` (see *Boot-state
   publication*) and carves a per-device DMA region from the top of RAM.
2. Builds the `virt`-board virtio-MMIO bus via the public
   `rustos_drv_bus_mmio::virtio_mmio_bus_from_dtb` constructor (the MMIO
   analogue of `rustos_drv_bus_pci::mechanism_one`; the concrete bus
   type stays crate-private behind `impl VirtioMmioBus`, §8) and
   provisions an `MmioTransport` through the `CAP_MMIO_MAP`-gated
   `KernelMmioMapper` (`kernel/virtio::provision_virtio_mmio`).
3. Walks the DTB for the PLIC base + `riscv,ndev` and the device's
   `interrupts` source, builds a `PlicController` + `IrqTable`, `arm`s
   the source, installs the S-mode trap dispatch (PLIC claim →
   virtio-MMIO `InterruptACK` → `IrqTable::fire` → complete), and calls
   `init_traps`.
4. Mints a `KernelVirtioHost` over the carved DMA pool and runs the
   shared `drive_driver_lifecycle` (`load → reload → device round-trip →
   unload`).

The completion park is a race-free `wfi`: the waiter unmasks the PLIC
source, clears `sstatus.SIE`, re-checks the line's ready flag, parks on
`wfi` only if still not ready, then restores `SIE`. Clearing `SIE` holds
a completion that lands in the check/`wfi` window *pending* (not taken)
so `wfi` observes it — no lost wake-up, no bounding timer. The
virtio-MMIO `InterruptACK` in the dispatch is load-bearing: a level-high
virtio-mmio source never re-edges, so without the ACK the device raises
no fresh interrupt for the next used buffer.

The `kernel/virtio` (`rustos-kernel-virtio`) crate holds the
architecture-neutral `KernelVirtioFactory` and the PCI/MMIO provisioning
walks so both the x86_64 (PCI) and riscv64 (MMIO) verticals reuse the
same code; it depends on no `kernel/arch/*` port (`AGENTS.md` §2.2, §6).

## Board model: `virt`

The runner targets QEMU's generic `virt` board (`qemu-system-riscv64 -M
virt`). Unlike x86_64 there is no firmware ISO step: `-bios default`
loads the OpenSBI firmware bundled with QEMU, which jumps to the ELF
supplied via `-kernel`. The kernel ELF is therefore the bootable
artifact directly — `Runner::run` passes `spec.kernel` straight through
to the riscv64 argv builder.

The `virt` board carries the devices the Stage 4.D drivers exercise: a
SiFive Test device, eight virtio-mmio transports, and a generic PCIe
host bridge. Every virtio-mmio transport is forced to the modern
(virtio 1.x, version 2) interface with `-global
virtio-mmio.force-legacy=false` — QEMU defaults to the legacy (version 1)
interface, but RustOS' `MmioTransport` only drives the modern layout. A
backing image attached with `Spec::with_virtio_blk`
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
`-global virtio-mmio.force-legacy=false`, `-kernel {elf}`, and one
`-drive if=none,format=raw,id=blkN,file=…` +
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
through `Runner::run` or `cargo xtask test --qemu` (which builds and
launches the enrolled `rustos-test-kernel-arch-boot-riscv64` bin for
`riscv64gc-unknown-none-elf`). A run can also be reproduced by hand:

```text
qemu-system-riscv64 -M virt -no-reboot -display none -serial stdio \
    -m 256M -smp 1 -bios default \
    -kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-kernel-arch-boot-riscv64
```

A clean boot prints the phase timeline and `id=4004 kernel boot
completed`, after which the `SiFive` Test finisher exits QEMU with
status `0`.
