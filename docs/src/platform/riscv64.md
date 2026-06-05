# riscv64

RustOS targets `riscv64gc-unknown-none-elf` as a Tier-1 platform. Two
halves exist today: the kernel-side **boot pipeline** that brings the
QEMU `virt` board up to `AuditEvent::BootCompleted`, and the host-side
**QEMU runner** that launches it (and the Stage 4.D virtio-MMIO
integration tests that build on the same harness). This page documents
both — the boot pipeline, the on-board boot model, the result protocol,
and the argv contract.

## Kernel boot pipeline

Like x86_64, `kernel/arch/riscv64` is a pure Arch HAL implementation
(`AGENTS.md` §17.2): it implements `rustos_arch_api::SchedulerArch`, the
monotonic clock, the hart-park primitive, the PLIC register driver, and
the S-mode trap glue, but it names no concrete kernel subsystem. The
boot pipeline that *does* name `kernel/{core,mem,sec}` and
`kernel/sched/api` lives downstream in the
`tests/integration/riscv64_boot` crate (`rustos-test-riscv64-boot`),
exactly as x86_64 keeps its boot pipeline and `BinArch` wrapper in the
downstream `rustos-kernel` crate. It boots to
`AuditEvent::BootCompleted` and is exercised by the
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
3. **Boot pipeline (`riscv64_boot::boot`).** Builds a `BootMemoryMap`
   reserving `[ram_base, __kernel_end)` (firmware + kernel image + boot
   heap) and marking `[__kernel_end, ram_end)` usable, constructs
   `RiscvArch` (`kernel_arch.rs`, the arch port's
   `rustos_arch_api::SchedulerArch` impl whose monotonic clock reads the
   `time` CSR via `rdtime`) wrapped in the downstream `RiscvBinArch`
   `kernel_core::KernelArch` adapter (orphan rules), assembles a
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

The Sv39 paging primitives, the context-switch primitive, the
supervisor-timer preemption surface, and the `ecall` syscall entry now
exist as host-tested arch primitives (Stage 3c — see *Stage 3
architecture primitives* below); they are not needed for the
boot-to-`BootCompleted` slice, which runs with paging off in a single
hart. Multi-hart SMP bring-up (and wiring the new address space and
context switch into the live scheduler) remain riscv64 follow-ups. The
ring-0 DTB virtio-mmio walk and the full device bring-up land in the
virtio-MMIO QEMU verticals (below).

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
- **Mask-before-wake.** `PlicController::mask` (inherent) masks a source
  by writing its PLIC priority register to zero (a single lock-free
  32-bit store) followed by a `SeqCst` fence — the riscv64 analogue of
  the x86_64 IO-APIC redirection-entry mask. The arch port owns no
  `kernel/irq` dependency, so the kernel-neutral `IrqController` bridge
  (`PlicIrqController`, in `tests/integration/riscv64_boot`) is what
  `IrqTable::fire` calls; it forwards to that inherent `mask`. See
  `docs/src/security/irq.md`.
- **S-mode trap vector.** `trap::init_traps` installs
  `rustos_riscv64_trap_vector` (`trap.s`) into `stvec` (direct mode)
  and enables `sie.SEIE` + `sstatus.SIE`. The vector saves the
  caller-saved registers into a `trap::TrapFrame` and passes its pointer
  to the Rust handler, which dispatches by `scause`: a U-mode `ecall`
  goes to the syscall path, a supervisor external interrupt forwards to
  the one-shot PLIC dispatch callback (claim → `IrqTable::fire` →
  complete), a supervisor timer interrupt drives the scheduler tick, and
  any other synchronous exception is forwarded to the installed
  `fault::FaultHandlerFn` (passing `scause`/`stval`/`sepc`) if one is
  present, otherwise fails closed (parks the hart).

## Stage 3 architecture primitives

These are the host-tested arch primitives the Stage-3 per-sub-stage
checklist requires (`PLAN.md` Stage 3). Each mirrors its x86_64
counterpart and keeps the pure bit/encoding math host-testable, gating
only the CSR/assembly operations to the freestanding riscv64 target.

- **Sv39 paging (`paging.rs`).** The three-level, 39-bit page-table
  primitives: PTE PPN encode/decode (`pte_from_phys` / `phys_from_pte`),
  per-level VPN extraction (`vpn_index`), the `satp` Sv39 selector
  (`satp_sv39`), a `.bss` `PageTablePool`, and an `AddressSpace` that
  identity-maps the low gigabytes with 1 GiB leaves, adds 4 KiB mappings
  through `map_4k`, and activates via `satp` + `sfence.vma` in `switch`.
  This is the architectural mechanism the memory-isolation vertical
  exercises: two hierarchies disagreeing on one VA so the MMU faults a
  cross-address-space access (`AGENTS.md` §4; see *Memory-isolation QEMU
  vertical* below).
- **Synchronous-exception hook (`fault.rs`).** A set-once fault handler
  (`FaultHandlerFn(scause, stval, sepc) -> !`, the page-fault `scause`
  constants, and `is_page_fault`) — the riscv64 analogue of the x86_64
  `idt` page-fault callback. The `trap` handler invokes it for an
  unexpected synchronous exception (reading `stval`/`sepc`) before
  falling back to parking the hart, so a kernel slice or test can decide
  what an otherwise-fatal fault means. The slot is set-once and a second
  publish fails closed (`AGENTS.md` §2.1).
- **Context switch (`context.rs` + `context.s`).** `TaskCtx { sp }` plus
  `rustos_arch_riscv64_switch`, which saves `ra` + `s0`–`s11` + `a0`
  onto the outgoing kernel stack, swaps `sp` through `TaskCtx`, and
  restores symmetrically. `TaskCtx::prepare` seeds a first-run frame
  (`ra = entry`, `a0 = arg`); a `const _` assert pins the 112-byte frame
  to a 16-byte multiple and `TaskCtx` to a single `sp` field at offset 0.
- **Supervisor-timer preemption (`preempt.rs`).** A set-once tick
  callback (`set_timer_callback`), the `sie.STIE` enable and the
  supervisor-timer `scause` decode, `interval_for_hz` (timebase →
  ticks), `init_local_preempt` (arm the SBI timer + enable `STIE`), and
  `on_timer_interrupt` (invoke the callback, then re-arm via SBI
  `set_timer`, which acknowledges `sip.STIP`). The kernel-side run-queue
  mutation stays in `kernel/sched::Scheduler::on_timer_tick`; this module
  only wires the riscv64 timer to it. The
  `tests/integration/timer_preempt_qemu_riscv64` QEMU vertical proves the
  path end-to-end: it arms the timer at 100 Hz and confirms the callback
  is driven repeatedly before reporting PASS.
- **`ecall` syscall entry (`syscall_entry.rs`).** riscv64 has no
  dedicated syscall instruction pair; a U-mode `ecall` raises a
  synchronous exception the trap handler routes here. `pack_raw_args`
  marshals `a0`–`a5` into the frozen `rustos_abi` `[u64; SYSCALL_MAX_ARGS]`
  layout (the same one x86_64 builds — `AGENTS.md` §2.2), `dispatch_ecall`
  forwards `(a7, &args)` to the set-once dispatch callback and writes the
  result into the frame's `a0`, and the handler advances `sepc` past the
  4-byte `ecall`. Absent a callback it fails closed. The
  architecture-neutral validation/capability/audit dispatcher lives in
  `kernel/syscall` and is installed by the downstream binary.

## Memory-isolation QEMU vertical

`tests/integration/memory_isolation_qemu_riscv64` is the riscv64
analogue of the x86_64 `tests/integration/memory_isolation` vertical and
the Stage-3 "memory-isolation test passes" deliverable (`AGENTS.md` §4).
It links only the arch port and supplies its own `kernel_main`:

1. Builds a **victim** and an **attacker** `paging::AddressSpace`, each
   identity-mapping the low 4 GiB (the board MMIO plus the RAM base at
   `0x8000_0000`). The victim additionally maps a 4 KiB secret frame at a
   virtual address (64 GiB) far above that window; the attacker does not.
2. Switches `satp` to the victim and reads the secret VA, confirming the
   mapping is genuine.
3. Installs an `on_fault` handler through `fault::set_fault_handler`,
   calls `trap::init_traps`, switches `satp` to the attacker, and reads
   the same VA.
4. The MMU raises a **load page fault** (`scause` 13); the trap vector
   routes it to `on_fault`, which asserts the cause is a load page fault,
   `stval` equals the secret VA, and the victim's frame is still intact
   at its physical address — then writes the `SiFive` Test PASS finisher.

A regression that fails to isolate the address never faults and trips a
per-site failure finisher instead (`AGENTS.md` §5.4.5 — fail closed).
The test is enrolled in `tools/xtask/src/commands/qemu_tests.rs` (single
CPU, 60 s budget).

## CC2 `abi-sys` `ecall` round-trip QEMU vertical

`tests/integration/abi_sys_syscall_qemu_riscv64` is the riscv64 half of
the `plans/CCOMPAT.md` stage CC2 per-native-target round-trip for the
C-callable syscall stub runtime (`lib/abi-sys`). Where x86_64 can drive
the stub straight from ring 0 (its `syscall` traps identically from any
privilege level), a riscv64 `ecall` only reaches the kernel's U-mode
syscall path when raised from U-mode (`is_ecall_from_user` matches only
`SCAUSE_ECALL_FROM_U`). The test therefore stands up a minimal U-mode
context with the Stage-3 Sv39 primitives:

1. Identity-maps the low 4 GiB (kernel code/stack, trap vector, MMIO) as
   S-mode-only.
2. Aliases the `ros_sys_cap_query` stub page at a high user VA with the
   `U` bit set (`flags::USER | READ | EXEC`) and maps a small user stack
   (`USER | READ | WRITE`). The stub is a self-contained leaf, so a
   single-page code alias is sufficient; the identity pages carry no `U`
   bit, so U-mode can reach only the aliased stub and its stack.
3. Installs the syscall dispatch callback (`syscall_entry::set_dispatch_callback`),
   points `stvec` at the trap vector (`trap::init_traps`), sets
   `sstatus.SUM` (so the S-mode trap handler may touch the U-bit stack),
   and `sret`s to U-mode at the aliased stub entry with the capability id
   in `a0`.

The stub's `ecall` raises an environment-call-from-U exception into the
S-mode trap vector, which marshals `a7`/`a0`–`a5` and calls the
callback; the callback asserts the dispatched `(number, args)` are
exactly what `ros_sys_cap_query` should have placed in the registers
before writing the `SiFive` Test PASS finisher. Any mismatch (or the
`ecall` resuming in U-mode at all) trips a distinct failure finisher
(`AGENTS.md` §5.4.5). Enrolled in `tools/xtask/src/commands/qemu_tests.rs`
(single CPU, 60 s budget); it runs under `cargo xtask test --qemu`, not
the host-only `cargo xtask ci` gate.

## Boot-state publication

`riscv64_boot::publish` exposes the boot-state a driver-bring-up
observer needs as set-once slots, the riscv64 analogue of the
`rustos-kernel` bin crate's `arch_wrapper` slots on x86_64. They live
beside the boot pipeline in the downstream `riscv64_boot` crate (not the
arch port) because publishing the firmware `BootMemoryMap` names
`kernel/mem`, which the HAL-only arch port must not (`AGENTS.md`
§17.2):

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
builds its own `PlicIrqController` + `IrqTable` over the DTB-discovered
PLIC base rather than reusing a `max_line == 0` kernel-core table.

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
   `interrupts` source, builds a `PlicIrqController` (the `IrqController`
   bridge wrapping the arch port's `PlicController`) + `IrqTable`, `arm`s
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

## Platform discovery (hardware tree)

The riscv64 port implements the Arch HAL `PlatformDiscovery` slice
(`AGENTS.md` §17.2 / §18.2) in `kernel/arch/riscv64::platform`. The
device-tree parser now lives once in the shared `lib/fdt` crate (§2.2);
`kernel/arch/riscv64::fdt` re-exports it so the boot path and the QEMU
integration tests keep naming `rustos_arch_riscv64::fdt::Fdt`.
`FdtDiscovery` normalises the two facts the reader extracts — the first
`/memory` region and the `/cpus` `timebase-frequency` — into the single
`lib/abi` hardware tree: a root node, a `Memory` node carrying the RAM
window as a capability-gated (`CAP_MMIO_MAP`) resource, and a `Timer`
node. It is host-tested against the shared DTB fixture and exercised by
the port's `passes_arch_hal_conformance_suite`.

## Per-CPU storage (`tp`)

The riscv64 port implements the Arch HAL `PerCpu` slice (`AGENTS.md`
§17.2) in `kernel/arch/riscv64::percpu_hal` over the **`tp`**
(thread-pointer) register — the conventional RISC-V per-hart anchor that
`boot.s` and `smp.s` already seed with the SBI-handed hart id before
entering Rust. `PerCpuStorage::read_self_base` / `write_self_base` are a
single `mv`-from / `mv`-to `tp`; the word is opaque (the kernel decides
whether it holds the hart id or a per-hart control-block address). On the
host build there is no `tp`, so the handle backs the word with an
in-handle cell solely for the round-trip + isolation conformance
verticals (`percpu::conformance`), folded into the port's
`passes_arch_hal_conformance_suite`.

## Interrupt controller (PLIC)

The riscv64 port implements the Arch HAL `IrqController` and
`InterruptEntry` slices (`AGENTS.md` §17.2 / `plans/WIRING.md` Stage W3)
on `kernel/arch/riscv64::plic::PlicController`, the same controller the
downstream `kernel/irq` bridge already drives. `IrqController::mask` /
`unmask` forward to the inherent priority-zero masking (mapping
`PlicError::SourceOutOfRange` to `IrqControlError::OutOfRange`), and
`InterruptEntry::claim` / `complete` forward to the PLIC claim/complete
register pair — PLIC source `0` ("no interrupt pending") maps to `None`.
Because the controller already abstracts its registers behind the
host-testable `PlicMmio` seam, the `plic_controller_passes_arch_hal_irq_conformance`
host test drives `irq::conformance::run_controller` (source 8 valid, 32
out of range) and `run_entry` over a real `PlicController` on a mock MMIO.

## Timer programming (`Timer`)

The riscv64 port implements the Arch HAL `Timer` slice (`AGENTS.md`
§17.2 / `plans/WIRING.md` Stage W4) in
`kernel/arch/riscv64::timer_hal` (struct `TimerHal`) over the
supervisor (SBI) timer wired in `kernel/arch/riscv64::preempt`.
`TimerHal::set_tick_callback` / `tick_callback` forward to the `preempt`
callback static, and `dispatch_tick` invokes it. The S-mode trap
handler's `preempt::on_timer_interrupt` dispatches each supervisor-timer
interrupt through `TimerHal::dispatch_tick`, so the callback invoke lives
in one place (§2.2); the SBI `set_timer` re-arm and the `sie.STIE` enable
stay in `preempt` (§2.4). On the host build the handle forwards to the
same static, so the `passes_timer_conformance` host test runs
`timer::conformance::run_all` over a real `TimerHal`. The
`timer_preempt_qemu_riscv64` vertical installs its tick callback through
`TimerHal` and stays green through the HAL.

## Context switch (`ContextSwitch`)

The riscv64 port implements the Arch HAL `ContextSwitch` slice
(`AGENTS.md` §17.2 / `plans/WIRING.md` Stage W5) in
`kernel/arch/riscv64::context_hal` (struct `ContextSwitchHal`) over the
bare-metal task-switch primitive in `kernel/arch/riscv64::context`
(`TaskCtx { sp }` + `context.s`'s `ra`/`s0`–`s11`/`a0` save/restore).
`ContextSwitchHal::prepare` seeds a never-run task's first frame and
`switch` performs the S-mode switch. The neutral `TaskContext` and the
port's `TaskCtx` are both a single `#[repr(C)]` `u64`, so the handle
reinterprets the pointer and forwards to `context` (a const-assert pins
the layout equality); the switch invoke lives in one place (§2.2). The
`prepare` contract is host-tested via `context::conformance::run_all`
(`passes_context_switch_conformance`); the switch itself is proven on the
bare-metal target by `sched_drive_qemu_riscv64`, so it carries no host
check (§2.1 — no fake primitive).
