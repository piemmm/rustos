# Next session — Stage 4.D Item 2-tail.2 QEMU validation +
# carried-over Items 2-tail.3 / 2-tail.4 / 3 / 4 / 5 / 6 + acceptance gate

## Where we are

`.junie/next-session-prompt.prev.md` (Item 2-tail.2 host-testable
substrate) landed in the preceding session and is recorded in
`PLAN.md` Stage 4.D under "Item 2-tail.2 — x86_64 IDT
external-vector + IO-APIC trap glue, *complete*". The frozen
`abi-v1` surface (`CapabilityId::IRQ_BIND`,
`SyscallNumber::IRQ_BIND` / `IRQ_WAIT`, `IrqHandle`,
`Errno::TimedOut`) is now backed end-to-end on x86_64 by:

- **Asm trap glue.** `kernel/arch/x86_64/src/external_irq.s`
  reserves IDT vectors `0x30..=0xFE` (207 vectors) for external
  IRQs through an `.altmacro` / `.rept` loop. Each per-vector
  stub pushes the vector immediate and jumps to a shared
  trampoline (`rustos_arch_x86_64_external_irq_common`) that
  saves the 15 GPRs in the `SavedRegs` layout, calls
  `rustos_arch_x86_64_external_irq_dispatch(*mut SavedRegs, u64)`,
  and `iretq`s. A `.rodata` `.quad` table publishes every stub
  address; the Rust side reads it through
  `kernel/arch/x86_64::irq::external_isr_addr`.
- **Routing + dispatcher.** `kernel/arch/x86_64::irq::routing`
  ships a lock-free `Routing` table backed by per-vector
  `AtomicU32` slots. The Rust dispatcher consults
  `Routing::gsi_for_vector`, forwards to the
  `ExternalIrqDispatchFn` installed via the set-once
  `set_external_irq_dispatch`, then writes the LAPIC EOI
  register. Both slots are one-shot publish (`AGENTS.md` §2.1).
- **IO-APIC controller.**
  `kernel/rustos-kernel::ioapic_controller::IoApicController<M>`
  generic over `IoApicMmio`, with per-pin `(vector, dest,
  masked)` caching and `mask` issuing a SeqCst fence after the
  volatile MMIO write. The mask-before-wake ordering is pinned
  by `ioapic_controller_mask_before_wake_ordering`.
- **KernelArch extension.** Two new trait methods on
  `kernel/core::bootinfo::KernelArch` with safe no-op defaults:
  `irq_routing(&self) -> IrqRouting` returns the routing the
  arch installed; `install_irq_dispatch(&self, &'static
  IrqTable)` is called immediately after the kernel-core
  `Phase::Irq` constructs the table.
- **`Phase::Irq`.** Inserted into `Phase::ORDER` strictly
  between `Sched` and `Syscall`. New host test
  `irq_phase_lands_between_sched_and_syscall` pins the
  ordering.
- **`BinArch` + `try_boot` wiring.** `BinArch::new` captures
  the `IrqRouting` and publishes the controller pointer into a
  `OnceCell` slot; `BinArch::install_irq_dispatch` publishes the
  IrqTable and installs `production_external_irq_dispatch`.
  `try_boot::discover_and_program_io_apics` walks every MADT
  `IoApic` entry, allocates one vector per pin from the
  reserved range, installs the per-CPU IDT entry, populates the
  routing table, and programs every redirection entry
  `masked = true`. Five new `BootError` variants (`NoIoApic`,
  `IrqVectorExhausted`, `IrqIdtInstall`, `IrqRoutingPublish`,
  `IrqProgramPin`) carry stable audit cause strings.
- **Docs.** `docs/src/security/irq.md` extended with a new
  "x86_64 trap glue (Stage 4.D Item 2-tail.2)" section and
  the controller table updated from "Not wired" to "Wired".
  `docs/src/architecture/kernel.md` boot-timeline table grew
  the `irq` row between `sched` and `syscall`.

Baseline at the start of this session: `cargo test --workspace`
(excluding the five QEMU-only integration test crates) → 766
tests green; `cargo clippy --workspace --all-targets -- -D
warnings` (same exclusion) clean; `cargo clippy -p
rustos-arch-x86_64 -p rustos-kernel --target x86_64-unknown-none
--features rustos-arch-x86_64/sched-arch -- -D warnings` clean;
`cargo fmt --check` clean; `cargo build -p rustos-kernel
--target x86_64-unknown-none` succeeds. Pinned toolchain is
`nightly-2026-05-27` (`rust-toolchain.toml`).

The preceding session was scoped under the user-confirmed "A2"
split: items 1–8 of the original Item 2-tail.2 plan landed and
were host-validated; item 9 (the QEMU integration test crate)
was deferred to this session because the previous environment
could not run QEMU. The user has now confirmed QEMU is
available on the Linux session host.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4.D status block — the two newest paragraphs
  ("Item 2-tail.2 — x86_64 IDT external-vector + IO-APIC trap
  glue, *complete*" and the superseded "Item 2-tail — kernel IRQ
  table + per-handle wait queue").
- This file.
- `.junie/next-session-prompt.prev.md` and
  `.junie/next-session-prompt.prev2.md` for the original Item
  2-tail.2 plan and the kernel-substrate split.
- `docs/src/security/irq.md` for the user-visible contract and
  the new "x86_64 trap glue" section.
- `kernel/arch/x86_64/src/{external_irq.s,irq.rs,irq/routing.rs}`
  for the asm thunks + Rust dispatcher + Routing table this
  session validates under QEMU.
- `kernel/arch/x86_64/src/{idt,apic,interrupts,acpi,preempt,
  percpu}.rs` for the existing per-CPU IDT install path
  (`percpu::install_vector`) and the LAPIC EOI helper.
- `kernel/rustos-kernel/src/{boot,arch_wrapper,ioapic_controller}.rs`
  for the production wiring `try_boot` now performs.
- `tests/integration/syscall_dispatch_qemu/{Cargo.toml,build.rs,
  src/main.rs}` as the layout template for the new
  `tests/integration/irq_qemu_x86_64` crate.
- `tools/qemu` for the QEMU launcher and exit-code conventions
  (the runner the new crate plugs into via `cargo xtask test`).
- `drivers/bus/virtio/src/kernel_host.rs` for the
  `KernelVirtioHost::notify_wait` polled shim Item 2-tail.3
  rewrites onto `IrqHandle`.
- `userland/system/drvhost/src/host.rs` for `VirtioHostFactory`
  and `HostConfig::virtio_host_factory` — drvhost stays free of
  `kernel/*` deps.
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs`
  (Item 3).

## What needs doing

### Item 2-tail.2 QEMU validation — `tests/integration/irq_qemu_x86_64`

The host-testable substrate is in place. This crate exercises a
real hardware interrupt end-to-end and is the missing piece of
Item 2-tail.2.

- **Crate layout.** Mirror `tests/integration/syscall_dispatch_qemu`:
  - `tests/integration/irq_qemu_x86_64/Cargo.toml` — a
    freestanding `[[bin]]` for `x86_64-unknown-none`, the same
    `test-hooks` default feature pattern, deps on
    `rustos-abi`, `rustos-arch-x86_64` (with `sched-arch`),
    `rustos-kernel`, `rustos-kernel-core`, `rustos-kernel-irq`,
    `rustos-kernel-sched`, `rustos-kernel-sec`,
    `rustos-kernel-sync`, `rustos-kernel-syscall`, `rustos-log`.
  - `tests/integration/irq_qemu_x86_64/build.rs` — verbatim
    copy of the syscall-dispatch one (linker script handoff).
  - `tests/integration/irq_qemu_x86_64/src/main.rs` —
    `#[no_mangle] extern "C" fn kernel_main` reuses
    `rustos_kernel::boot` with a custom audit sink. The sink
    installs an `IrqTable`-backed handler that observes
    `IrqWaitOk` and flips `qemu_exit::exit_success`; any
    other outcome flips `exit_failure`.
- **Synthetic IRQ source.** Use the **PIT channel 0** at the
  legacy IRQ 0 GSI (typically GSI 2 after the MADT
  `InterruptSourceOverride` for `source = 0`). Program it as a
  one-shot via the IO ports 0x40/0x43; the kernel boot pipeline
  has already programmed the IO-APIC redirection entry masked,
  and the QEMU test unmasks it through
  `IoApicController::program_pin(gsi, vector, dest, /* masked =
  */ false)` before arming the PIT. `irq_wait` is invoked from a
  synthetic in-kernel task wired via the same
  `KernelSyscallHandlers` quartet the syscall-dispatch crate
  synthesises; the test asserts the handler observes
  `WaitStep::Ready` within the documented timeout window.
- **Mask-after-wake observation.** After the IRQ fires the test
  re-reads the IO-APIC redirection entry's low half through the
  arch crate's `VolatileIoApicMmio::read` and asserts the mask
  bit (bit 16) is set — the kernel-side
  `IoApicController::mask` was supposed to mask the line during
  `IrqTable::fire`. A failure here would be a regression in the
  mask-before-wake ordering invariant; the QEMU output must
  carry a stable failure string the test runner keys off.
- **QEMU launcher.** Add the crate to `tools/qemu`'s known-bins
  table (the existing `qemu_exit::exit_*` conventions are
  already plumbed). Use `-no-reboot -no-shutdown` so the
  isa-debug-exit code is the only success path.
- **Workspace registration.** Add the crate to
  `Cargo.toml::[workspace].members` and to the `cargo xtask
  test --qemu` workload list.
- **Coverage.** ≥ 75 % for the new crate per `AGENTS.md` §7;
  the test exercises one end-to-end path which is exactly
  what the integration tier owes.

### Item 2-tail.3 — `KernelVirtioHost::notify_wait` rewrite

Once a real `fire` source exists (it does, since the preceding
session), the polled cooperative shim in
`drivers/bus/virtio/src/kernel_host.rs::KernelVirtioHost::notify_wait`
is no longer the only available wake-up path. Land:

- A per-virtio-host pre-bound `IrqHandle` (the bus-driver
  registration path — Item 3 below — supplies the GSI).
- `notify_wait` now calls into the kernel IRQ subsystem to
  block until the bound handle fires. The polled log accessor
  is retained only on `MockHost`; the production path becomes
  the canonical wake-up.
- Tests covering: virtio-host blocks on a fresh `IrqHandle`,
  `fire(gsi)` from a trap dispatcher releases the wait, mask
  is honoured before the driver observes the wake-up.

### Item 2-tail.4 — Kernel-binary `VirtioHostFactory` impl

Once the kernel binary has a per-process `DmaPool` (the Stage
4.D Item 1 work that lands in parallel), install a
`VirtioHostFactory` in the kernel binary that mints a fresh
`KernelVirtioHost` per loaded driver and passes it through
`HostConfig::virtio_host_factory`. The drvhost seam already
accepts it. **Do not** add a `kernel-host` feature to
`userland/system/drvhost`; the factory abstraction is designed
so drvhost stays free of `kernel/*` deps.

### Item 3 — Bus-handle hand-off from `drivers/bus/{pci,mmio}`

(Unchanged from the prior prompt — reproduced for continuity.)

- Extend the `PciBackend` / `MmioBackend` constructors in
  `drivers/bus/virtio` to receive a capability-checked register
  window rather than the bare identification tuple they carry
  today.
- The PCI and MMIO bus drivers obtain the window from the kernel
  via the DMA / future MMIO-map facility (the *kernel* allocates
  the window; the bus driver does not synthesise pointers).
- Per-bus unit tests with mock register windows; a QEMU
  integration test that walks PCI / DTB and hands a working
  window through to the virtio transport.
- Update `docs/src/drivers/bus.md` with the hand-off sequence
  and the capability flow.

### Item 4 — QEMU integration tests

Once Items 2-tail.3 / 2-tail.4 / 3 are in place:

- `tests/integration/virtio_blk_pci_x86_64` — boots the kernel +
  driver host + signed `.rxe`, attaches `virtio-blk` to a backing
  qcow2, reads sector 0 (planted by `tools/qemu`), writes a known
  pattern to sector 1, reads it back, verifies checksum.
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP echo
  round-trip against `qemu user net`'s built-in DHCP/ARP/ICMP
  responder. Depends on Item 5.
- Add an unload → reload → reuse test for each driver. The
  drvhost `VirtioHostFactory` is the seam through which a fresh
  `KernelVirtioHost` (and therefore a fresh per-driver
  `DmaPool`) is minted on each load.

### Item 5 — Userland ARP / IP / ICMP responder

The virtio-net QEMU integration tests need a small userland
stack:

- New crate `userland/net/icmp/` implementing only ARP request +
  reply, IP + ICMP echo, and a minimal main loop sitting on top
  of the `Net` trait.
- Out of scope: TCP, UDP, IPv6, routing — those are Stage 6 work.

### Item 6 — Acceptance gate

After Items 2-tail.2-QEMU / 2-tail.3 / 2-tail.4 / 3 / 4 / 5
land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7
  (`userland/net/icmp`, the four new virtio QEMU integration
  crates, the new `tests/integration/irq_qemu_x86_64`).
- Confirm `kernel/sec`, `kernel/ipc`, and `kernel/irq` coverage
  remain ≥ 95 % after every addition.

## Toolchain note

The pinned `nightly-2026-05-27` is required for
`kernel/arch/x86_64` (`#[unsafe(naked)]`, inline-const). On
systems without `rustup` on PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`.

## Assumptions for the next session to confirm at the top of the PR body

1. The `abi-v1` surface remains frozen: `CapabilityId::IRQ_BIND`,
   `SyscallNumber::IRQ_BIND` / `IRQ_WAIT`, `IrqHandle`,
   `Errno::TimedOut`, the two `SyscallSpec` rows, the refreshed
   `SYSCALL_TABLE_HASH`, **and** the new kernel-internal
   `KernelArch::irq_routing` / `KernelArch::install_irq_dispatch`
   hooks (both have safe no-op defaults; any future arch port
   that overrides them must update `docs/src/security/irq.md`'s
   per-arch controller table in the same commit). This session
   does not mutate the user-visible `abi-v1` surface; any
   departure is an `abi-v2` change and is out of scope.
2. The `kernel/irq` substrate (`IrqTable`, `IrqController`,
   `UnsupportedController`, `UNSUPPORTED_CONTROLLER`) is
   unchanged. The x86_64 trap glue + IoApicController *composes*
   it; this session validates the composition under QEMU.
3. The `Phase::Irq` init step preserves the existing ordering —
   Log / Mem / Sec / Sched / **Irq** / Syscall / Ipc — so the
   `irq_qemu_x86_64` audit sink keys off exactly two new
   `KERNEL_PHASE_STARTED` + `KERNEL_PHASE_READY` records
   carrying `phase = "irq"` strictly between the `sched` and
   `syscall` pairs.
4. The `irq_wait` polling loop is **not** replaced with a
   parking blocker in this session. The reason is documented
   in `docs/src/security/irq.md` ("Wait semantics"): the
   park-based blocker needs a table-internal interlock to
   close the lost-wakeup race between `fire` and `park`. That
   work is its own follow-up.
5. The mask-before-wake invariant remains the load-bearing
   safety property. The x86_64 `IoApicController` implementation
   honours the same ordering: `IrqTable::fire` calls
   `controller.mask(line)` *before* setting `ready`; the
   `IoApicController` mask write is a volatile store followed
   by a SeqCst memory fence so a subsequent waker observing
   `ready = true` is guaranteed to also observe the mask.
   The QEMU integration test re-reads the IO-APIC redirection
   entry after wake-up and asserts the mask bit is set; any
   regression that violates the ordering must fail the new
   test.
6. `KernelVirtioHost::notify_wait` blocks on `IrqHandle` in the
   production path; the polled log accessor is retained only on
   `MockHost`.
7. The kernel-binary `VirtioHostFactory` impl lives in
   `kernel/rustos-kernel` (or a thin kernel-side adapter crate
   if the generic bounds force it); `userland/system/drvhost`
   stays free of `kernel/*` deps.

## Verification commands for the QEMU test crate

```
# Build the crate freestanding.
cargo build -p rustos-test-irq-qemu-x86-64 \
    --target x86_64-unknown-none

# Run it through tools/qemu.
cargo xtask test --qemu --bin rustos-test-irq-qemu-x86-64

# Full acceptance.
cargo xtask ci
cargo xtask test
```
