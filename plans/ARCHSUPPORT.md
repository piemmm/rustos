# ARCHSUPPORT.md — x86_64 product parity with aarch64

This is the staged plan for bringing the `x86_64-unknown-none` port to the
same **product** state the aarch64 port reached through `plans/PI.md`: a
bootable image, the encrypted-root unlock → `/System` mount → on-disk app
store → users DB → login/session pipeline, `devmgr` autoload in the
production boot, display/seat/console wiring, and the QEMU vertical parity
sweep — ending with the deletion of the embedded spawn registry (`PLAN.md`,
"Self-contained bundles", increment 5). It is **binding under `AGENTS.md`**
— read `AGENTS.md` (especially §2, §4, §5.4, §12, §16, §17, §18), `PLAN.md`,
`plans/WIRING.md` (Arch HAL parity — already complete for x86_64),
`plans/PI.md` (the aarch64 template this plan replays over PCI/ACPI),
`plans/SPAWN.md`, `plans/USERS.md`, and `plans/DISPLAY.md` first; every rule
in all of them applies here without exception.

## Ledger

| Id | Item | Status |
|---|---|---|
| A1 | x86_64 whole-disk image builder | planned — delivered by `plans/BOOTLOADER.md` B4 |
| A2 | Production boot storage floor + registry deletion | in progress — the deletion is blocked on A1 and the riscv64 image |
| A3 | Interrupt-driven COM1 console + login/session supervision | done |
| A4 | `devmgr` autoload over the ACPI/PCI tree | in progress |
| A5 | Boot display, seat registry, graphical session | planned — delivered by `plans/FINISH-x86_64.md` |
| A6 | QEMU vertical parity sweep + docs | planned |
| A7 | ACPI power-off | planned |
| A8 | x86_64 hardening unblock | planned |

## 0. Scope and decisions (binding for this plan)

- **This is wiring, not new subsystems.** At the kernel/Arch-HAL tier the
  x86_64 port is the `plans/WIRING.md` reference port and its parity matrix
  is full. Everything this plan lands — the unlock composition
  (`root_mount::mount_root_disk_and_load_users`, PBKDF2, the FAT
  `root.unlock` descriptor), `lib/partition` (written for "a UEFI x86_64 GPT
  disk on any arch"), `lib/appload`, `lib/users`, `lib/devmatch`,
  `lib/fbcon`, `lib/seat`, the volume service — is already arch-neutral,
  host-proven, and exercised by the aarch64 production boot. Re-deriving any
  of it per-arch is the §2.2 duplication this plan forbids: each increment
  admits the one shared definition from the x86_64 boot, and only the
  genuinely target-divergent glue (PCI/ACPI discovery emission, the
  BIOS/UEFI image layout, COM1/VESA plumbing) is new code (§2.21).
- **The finish line is a deletion.** The embedded spawn registry
  (`SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s except PID 1 `init`,
  `spawn_paths.rs`, `program_manifests.rs`) exists today *only* as the
  x86_64/riscv64 §18.6 boot floor. When the storage floor and image layout
  land for both remaining disk-booting ports, the registry is deleted in
  that same change (§2.14) — never kept "just in case". riscv64 rides A2's
  shared work in the same increment or its own immediately after; wasm32 has
  no disk floor and is out of scope here.
- **The boot floor binds by discovery-match, never by assumption (§18.6).**
  The x86_64 root storage path is the existing in-kernel
  `tairix_drv_storage_virtio_blk` floor entry over the virtio-PCI
  provisioning seam (`kernel/tairix-kernel/src/x86_64/virtio_boot.rs`),
  bound because the ACPI/PCI-enumerated hardware tree matched its bind
  table through the shared `lib/devmatch` policy — no compile-time device
  address, no `cfg` fork outside the port (§2.20).
- **Each increment lands complete and green.** No "for now" shims (§2.19):
  an increment that wires the unlock pipeline wires the whole pipeline
  (unlock kthread → ARXFS root mount → users DB publish → `/System` mount
  → services/`login` off disk), with its QEMU vertical, docs, and the full
  §7 whole-project gate. Verticals reuse the shared scenario crates the
  aarch64 bins wrap ("thin new bin over shared scenario code"), never a
  second scenario implementation (§2.2).
- **Target environment is QEMU/UEFI-class PC hardware.** Nothing here is
  metal-gated the way the Pi's EMMC2/VL805 work was; the acceptance
  environment is QEMU `q35`/`pc` with virtio-PCI devices, VESA/GOP display,
  PS/2 + virtio input, and COM1. Real-PC breadth (AHCI, NVMe, USB boot,
  broader GOP modes) is deliberately out of scope for this plan and stays
  with its own driver plans (`plans/DEVICES.md`, `plans/USB.md`,
  `plans/NETWORK.md`).
- **Hardening debt is tracked, not hidden.** KPTI and the x86_64
  speculation barriers (IBRS/IBPB/STIBP/SSBD) are `Pending` in the port's
  honest §19.1 profile. They waited on the Stage 6 user/kernel page-table
  boundary, which has landed, and carry "[DO IMMEDIATELY ON UNBLOCK]" in
  `PLAN.md` (§19 item 10). That work is A8 here so this plan cannot be called
  done while the profile still says `Pending` — but it is tracked separately
  because it is not an aarch64-parity item (aarch64 carries its own `Pending`
  KPTI/Spectre-v2 rows).

## 1. Current state (what already exists — do not rebuild)

- **Arch HAL: full parity.** Every §17.2 slice — ACPI `PlatformDiscovery`,
  `PerCpu`, `IrqController` (IOAPIC), `Timer`, `ContextSwitch`, MMU
  `AddressSpace`, local + cross-CPU TLB shootdown, INIT-SIPI-SIPI
  `SecondaryBringup`, `iretq` `EnterUser`, side-channel/memtag profiles —
  is implemented and conformance-gated (`plans/WIRING.md` §1).
- **Kernel verticals green on x86_64:** memory_isolation, enter_user,
  preempt_el0, uaccess_fault, syscall_regs, spawn_program/init/session/
  el0_*, stack_guard/overrun/grow, kthread_switch, c_program, wait,
  mem_map, cross_cpu_tlb_shootdown, scheduler_stress.
- **Drivers under test:** `virtio_blk_pci_x86_64`,
  `netstack_autoload_qemu_x86_64` (the two-process network vertical),
  `fat32_virtio_blk_pci_x86_64`, `arxfs_virtio_blk_pci_x86_64`,
  `vesa_display_qemu_x86_64`, `ps2_input_qemu_x86_64`, `irq_qemu_x86_64`;
  the virtio-PCI provisioning seam and an in-kernel driver host
  (`x86_64/driver_host.rs`) exist.
- **Live boot verticals over the production x86_64 pipeline:**
  `root_unlock_login`, `users_db`, `root_unlock_admission`, `spawn_session`,
  `autoload_input` (virtio keyboard), `netstack_autoload`/`_static`/`_dhcp`/
  `_dhcp6`/`_bond`, and `audio_virtio` (unlock → login → shell → `devmgr`
  autoload → `audiod`).
- **Production boot gap:** `kernel/tairix-kernel/src/x86_64/boot.rs` composes
  the whole storage/store/users/volume pipeline, but has no boot display and
  no `with_seat_registry` (A5, `plans/FINISH-x86_64.md`), and it still spawns
  from the embedded registry (A2).

## 2. Increments (dependency order; each fully gated per §7)

### A1 — `tools/mkimage` x86_64 image builder

The Stage 8 deliverable `images/tairix-x86_64.iso` / bootable disk image
(§12): GPT layout, a FAT/ESP boot partition carrying the loader + kernel +
the `root.unlock` descriptor, and an encrypted ARXFS root with the §16
skeleton — reusing the existing pure-Rust rootfs/partition/appload planting
code (`build_system_partition`, the `image_apps`/`image_drivers` pipelines)
unchanged.

**The §15.7 question this plan flagged is answered: genuine BIOS/UEFI boot
needs a boot loader, and TAIRiX ships a first-party, Rust-only one rather
than GRUB (forbidden C / external code). That work is its own binding plan,
`plans/BOOTLOADER.md`** — the pure loader core `lib/bootload` (ELF →
`LoadPlan`, landed as B1) plus the per-firmware `boot/*` shells, handing off
through the kernel's existing multiboot2 entry. The GPT + ESP whole-disk
builder this A1 describes is **`plans/BOOTLOADER.md` B4** (it needs GPT
encode in `lib/partition` and the UEFI shell that boots from the ESP);
A1 is delivered *by* B4, so this row tracks it there rather than duplicating
the design. Deliverables (at B4): the `--target x86_64` builder in
`tools/mkimage` with `installer`/`debug` profiles matching the Pi builder's
semantics, host tests over the produced layout, and the whole-disk OVMF
fixture that boots the produced image with no `-kernel`. QEMU's `-kernel`
PVH path remains the fast, firmware-free test path the existing x86_64
verticals use.

### A2 — Production boot storage floor + registry deletion

The production boot composes the shared storage pipeline and is proven on
live guest boots. What it guarantees:

- `boot_x86_64::seed_hardware_tree` returns the collected tree by value;
  `try_boot` hands it to `unlock_service::record_boot(/* dtb */ 0, tree, log)`,
  which resolves the bootstrap root block binding through
  `root_storage::resolve_root_block_driver`, stashes it, and moves the tree
  into `HW_TREE`. The dtb is `0` because the x86_64 bring-up re-resolves the
  transport from PCI config space, not a firmware device tree.
- `try_boot` composes the shared pipeline exactly as the aarch64/riscv64
  boots do: `with_app_store` / `with_users_db` / `with_users_admin` /
  `with_filesystem` / `with_volumes` / `with_volume_service`.
- `kernel/tairix-kernel/src/x86_64/root_unlock.rs` is the port's unlock
  admission (`spawn_if_present` at the init seam, `virtio_blk_unlock`):
  it brings the bound virtio-blk-PCI root up over `mechanism_one` +
  `provision_virtio_pci`, routes the device's interrupt through **MSI-X**
  (binding the discovered PCI Interrupt-Line GSI, reusing its boot-assigned
  vector), parks on an `IrqParkWaiter`, and hands the opened `VirtioBlk` to
  the shared `unlock_orchestrate::finish_unlock`. The passphrase is read from
  the interrupt-driven COM1 console (A3).
- `IoApicController::rearm` unmasks the line, so a user-space INTx `irq_wait`
  re-arm re-enables its pin.
- Live verticals, each a thin bin over the shared virtio-PCI bring-up
  (`run_virtio_pci_scenario` in `tests/integration/virtio_qemu_support`) and a
  transport-generic scenario tail the aarch64 siblings share:
  `root_unlock_login_qemu_x86_64` (the root-mount → login policy) and
  `users_db_qemu_x86_64` (the boot-time users-database read over a plaintext
  users-root volume). The encrypted-root fixture's `/System` partition is
  sized from its planted content by the policy and assembly the Pi image uses
  (`tairix_syshelp::build_system_volume`, `tairix_syshelp::assemble_disk`).

Remaining: the A1 image builder, then — once the riscv64 image also exists —
deleting `SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s (all but PID 1 `init`),
`spawn_paths.rs`, and `program_manifests.rs` (§2.14).

### A3 — Interrupt-driven console + login/session supervision

COM1 is an interrupt-driven, lossless console, and `init` supervises the login
session over it. What it guarantees:

- `tairix_arch_x86_64::serial` carries the 16550 receive primitives
  (`read_console_bytes`, the receive-interrupt enable/disable, host-tested
  `lsr_data_ready`/`ier_with_rx_*`). `kernel/tairix-kernel/src/x86_64/com1_rx.rs`
  carries the `RflagsIrqControl` receive gate, the `COM1_INPUT` queue,
  `Com1ConsoleRead`, `enable_uart_console_irq` (device IER + IO-APIC unmask)
  and the device-IER flow-control brake. The COM1 GSI comes from the MADT
  interrupt-source override for ISA IRQ 4, else identity.
- `serial_sink` installs the read half gated on the unlock service's ownership
  latch, so `login` never races the unlock kthread for console-0 input.
  `root_unlock`'s `X86UnlockConsole` arms the receive interrupt and hands the
  interactive read half to the unlock kthread.
- **The console GSI never reaches the `irq_wait` table.** `IrqTable::fire`
  masks a line before it finds the line unbound, and COM1's line is unbound by
  design, so `production_external_irq_dispatch` drains the FIFO and returns,
  latching the reschedule, as the aarch64 UART dispatch does.
- The backpressured FIFO → `ConsoleInputQueue` drain is one shared definition
  (`console_uart::drain_fifo_into_console`) used by the x86_64 16550 and the
  aarch64 PL011; only the per-UART FIFO read, latch clear and brake are
  injected.
- PCI discovery (`boot_x86_64::seed_virtio_pci`) prefers ECAM when the firmware
  advertises an MCFG (`q35`, real UEFI/PCIe) and otherwise uses mechanism #1
  (CF8/CFC), over one `probe_virtio_pci`.
- Live verticals: `root_unlock_admission_qemu_x86_64` (interactive passphrase →
  `/System` mount → encrypted-root unlock → users database installed, over the
  virtio-blk-PCI MSI-X completion path), `spawn_session_qemu_x86_64` (the
  `wait` → reap → relaunch supervision cycle), and `audio_virtio_qemu_x86_64`
  (a scripted passphrase, `login` form and shell command).

### A4 — `devmgr` autoload over the ACPI/PCI tree

The x86_64 discovery emits generic match-key nodes (PCI
`vendor:device:class`, virtio ids) that `devmgr` matches against the signed
driver store through the same `lib/devmatch` policy as every port; only the
per-port node emission is new. Live today: the virtio-input keyboard
(`autoload_input_qemu_x86_64`), virtio-net (`netstack_autoload_qemu_x86_64`
and the `netstack_*` family) and virtio-sound (`audio_virtio_qemu_x86_64`),
each autoloaded into its own process in the production boot.

Remaining:

- The verticals `devmgr_hwtree_qemu_x86_64`, `driver_spawn_qemu_x86_64` and
  `driver_unload_qemu_x86_64`.
- PS/2: `drivers/input/ps2` has no bind table and discovery emits no i8042
  node, so it is reachable only through the in-kernel host
  (`ps2_input_qemu_x86_64`). Its node comes from the ACPI namespace (`PNP0303`),
  so it waits on an AML reader (`plans/FINISH-x86_64.md` S4).
- The virtio pointer lands with `plans/FINISH-x86_64.md` F6.

### A5 — Boot display, seat registry, graphical session

Delivered by `plans/FINISH-x86_64.md` (F1–F8), which owns the design: a UEFI
GOP framebuffer handed over by the first-party loader, the shared boot console
and console/seat layout, write-combining on x86_64, and the greeter and
desktop verticals.

### A6 — QEMU vertical parity sweep + docs

The remaining aarch64-only verticals gain x86_64 siblings (thin bins over
the shared scenario crates): `sandbox`, `heap`, `file_map`, `mmio_map`,
`mem_pin`, `memsoak`, `service_ceiling`/`session_ceiling`, `irq_kthread`,
`preempt_inkernel`, `sched_drive`, `ipi_smp`,
`timer_preempt` — audited against the then-current inventory when A6
starts; any vertical exercising a feature a prior increment wired lands
with that increment instead. Same-change docs: `docs/src/platform/` x86_64
page brought to the aarch64 page's level, README feature/architecture
matrix rows updated (§13).

### A7 — ACPI power-off

`system_power` (`abi-v1` 105, `CAP_SYSTEM_POWER`) restarts an x86_64 machine
through the legacy PC reset hardware (the 8042 pulse-reset, then the `0xCF9`
reset-control register), but **power-off answers `NotSupported`** and leaves
the machine running: the port keeps the Arch-HAL `poweroff` default rather
than poking a guessed chipset control port. Closing it needs an ACPI
power-management path that parses the FADT (and the DSDT `\_S5` object) for
the PM1a/PM1b control block and sleep-type values, then writes the `SLP_TYP`
+ `SLP_EN` sequence — real firmware parsing, not a constant. aarch64 (PSCI
`SYSTEM_OFF`) and riscv64 (SBI SRST) already power off. Vertical: a QEMU
x86_64 scenario asserting the guest exits on `PowerOff`, mirroring the
equivalents on the two ports that have it.

### A8 — x86_64 hardening unblock

KPTI + IBRS/IBPB/STIBP/SSBD move from `Pending` to `Supported` in the
port's §19.1 profile, with the side-channel conformance vertical proving
the barriers. The Stage 6 user/kernel page-table boundary they waited on
has landed, so the "[DO IMMEDIATELY ON UNBLOCK]" order in `PLAN.md` now
applies. Tracked here so this plan is not "done" while the profile is
`Pending`; tracked separately because it is not an aarch64-parity item.

## 3. Invariants (hold across every increment)

- No `cfg(target_arch …)` outside the §17.2 allow-list; all new per-port
  code lives under `kernel/tairix-kernel/src/x86_64/` or
  `kernel/arch/x86_64/` (§2.20, §17.2).
- Shared logic is hoisted, never copied: any routine an increment needs
  that aarch64 already has in a per-port file is moved to the shared home
  in that increment, with both ports depending on the one definition
  (§2.21) — the aarch64 `root_unlock`/boot composition is refactored
  toward shared code wherever the two ports would otherwise twin.
- Every boot admission is capability-gated and fails closed (§5.4);
  unlock, mount, store, and autoload decisions log stable §19.4 event IDs
  identical to the aarch64 events (one definition).
- Each increment runs the full §7 gate (fmt, `cargo xtask ci` once,
  `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both --secs 20`) and
  moves its ledger row, with its prose in the done-state summary form (§13).
