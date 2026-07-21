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
  honest §19.1 profile, blocked on the Stage 6 user/kernel page-table
  boundary and marked "[DO IMMEDIATELY ON UNBLOCK]" in `PLAN.md`. That work
  is A7 here so this plan cannot be called done while the profile still says
  `Pending` — but it is gated separately because its blocker is not an
  aarch64-parity item (aarch64 carries its own `Pending` KPTI/Spectre-v2
  rows).

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
- **Drivers under test:** `virtio_blk_pci_x86_64`, `netstack_pci_x86_64`,
  `fat32_virtio_blk_pci_x86_64`, `arxfs_virtio_blk_pci_x86_64`,
  `vesa_display_qemu_x86_64`, `ps2_input_qemu_x86_64`, `irq_qemu_x86_64`;
  the virtio-PCI provisioning seam and an in-kernel driver host
  (`x86_64/driver_host.rs`) exist.
- **Production boot gap (the whole point of this plan):**
  `kernel/tairix-kernel/src/x86_64/boot.rs` wires only COM1 consoles,
  installed memory, `LATE_IDENTITY`, init spawn, the embedded-registry
  spawn producer, and the ACPI hardware-tree source. It has no
  `root_unlock` module, no `with_app_store`/`with_users_db`/
  `with_filesystem`/`with_volumes`/`with_seat_registry`, and no `devmgr`
  autoload admission — all of which the aarch64 boot
  (`src/aarch64/{boot,root_unlock}.rs`) already composes from shared code.

## 2. Increments (dependency order; each fully gated per §7)

### A1 — `tools/mkimage` x86_64 image builder (`planned`; now on `plans/BOOTLOADER.md`)

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

### A2 — Production boot storage floor + registry deletion (`in progress`)

The boot composition is **landed and host-gate-green** (the live QEMU
verticals + registry deletion remain, gated on the A1 image builder —
staged exactly as the riscv64 parity port was: production composition
host-gate-green first, then boot-confirmed). Done-state:

- `boot_x86_64::seed_hardware_tree` returns the leaked `&'static [HwNode]`
  tree it publishes to `HW_TREE`; `try_boot` resolves the bootstrap root
  block binding from it through the shared `root_storage::
  resolve_root_block_driver` gate and stashes it with
  `unlock_service::record_boot(binding, /* dtb */ 0, tree)` — dtb is `0`
  because the x86_64 bring-up re-resolves the transport from PCI config
  space, not a firmware device tree.
- `try_boot` composes the shared pipeline exactly as the aarch64/riscv64
  boots do: `with_app_store` / `with_users_db` / `with_users_admin` /
  `with_filesystem` / `with_volumes` / `with_volume_service`.
- `kernel/tairix-kernel/src/x86_64/root_unlock.rs` is the port's unlock
  admission (`spawn_if_present` at the init seam, `virtio_blk_unlock`):
  it brings the bound virtio-blk-PCI root up over `mechanism_one` +
  `provision_virtio_pci`, routes the device's interrupt through **MSI-X**
  (binding the discovered PCI Interrupt-Line GSI, reusing its boot-assigned
  vector), drives an `IrqParkWaiter` (with a `sti;hlt;cli` fallback park),
  and hands the opened `VirtioBlk` to the shared
  `unlock_orchestrate::finish_unlock`. `x86_64/init_spawn.rs` calls
  `spawn_if_present(ctx)` before `admit_init`. The console-0 read half is
  the fail-closed `NULL_CONSOLE_READ` this slice (interactive COM1 input is
  A3), so `login` fails closed while the disk still mounts and the driver
  store still serves.
- `IoApicController::rearm` now unmasks the line (the riscv64-class
  re-arm fix), so a user-space INTx `irq_wait` re-arm re-enables its pin.

Two live boot verticals have now landed. **`root_unlock_login_qemu_x86_64`
passes a real guest boot** — the first live-boot exercise of the x86_64
root-mount->login *policy* over the virtio-**PCI** bus. It is a thin bin over
the shared virtio-PCI bring-up (`run_virtio_pci_scenario`) and the shared
`root_unlock_login` scenario tail — the same tail the aarch64 vertical runs,
hoisted into `tests/integration/virtio_qemu_support` and made generic over the
transport so both ports drive one definition (§2.2). Authoring it surfaced and
fixed a fixture scaling defect: the encrypted-root fixture's `/System` `ARXFS`
partition was a fixed 32 MiB that the (larger) x86_64 bundle set overflowed;
`tairix_test_encrypted_root_image` now sizes that partition from the planted
content (`system_sectors_for`, never below the 32 MiB floor) and derives the
root LBA / total from the produced image, so one fixture serves every arch
(§24.1) and `qemu_tests.rs` reads each image's true sector count.

**`users_db_qemu_x86_64` passes a real guest boot too** — the first live-boot
exercise of the x86_64 boot-time users-database read path over virtio-**PCI**.
It is the thin-bin x86_64 sibling of `users_db_qemu_aarch64`: the same shared
virtio-PCI bring-up plus the transport-generic `users_db_load` scenario tail
(one definition, §2.2), over a planted plaintext users-root ARXFS volume
(`FsDisk::UsersRoot`); it mounts the volume, runs
`tairix_kernel_core::load_users_db`, and proves the parsed database
authenticates the planted account while a wrong password is refused. No
production code changed — the whole x86_64 users-database path was already in
place. Being plaintext it needs no passphrase, so unlike the admission
vertical below it does not depend on interactive console input.

Remaining for A2: the A1 image builder, and — once A1 lands for both remaining
disk-booting ports — deleting `SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s (all
but PID 1 `init`), `spawn_paths.rs`, and `program_manifests.rs` (§2.14).
**`root_unlock_admission_qemu_x86_64` (the production kthread-admission path)
is deferred to A3**, not A2: the production unlock kthread reads the passphrase
from the fail-closed `NULL_CONSOLE_READ` this slice, so an interactive
passphrase prompt — and hence the `USERS_DB_INSTALLED_MESSAGE` witness the
aarch64 admission vertical keys on — is impossible until A3 wires
interrupt-driven COM1 input. It is therefore *not* a thin bin over the shared
scenario; it is a live exercise of the A3 console and belongs with A3.

### A3 — Interrupt-driven console + login/session supervision (`in progress`)

**The interrupt-driven COM1 console is implemented and host-tested**; its
live verticals are blocked on `plans/OPEN-DEFECTS.md` D7 (a separate A2
MSI-X defect), not on the console work. Done-state:

- COM1 receive is interrupt-driven, replacing the fail-closed
  `NULL_CONSOLE_READ`: `tairix_arch_x86_64::serial` gained the 16550 RX
  primitives (`read_console_bytes`/`enable`/`disable_rx_interrupt`, pure
  `lsr_data_ready`/`ier_with_rx_*` helpers, host-tested);
  `kernel/tairix-kernel/src/x86_64/com1_rx.rs` carries the `RflagsIrqControl`
  receive gate, the `COM1_INPUT` queue, the poll-backed `Com1ConsoleRead`,
  `enable_uart_console_irq` (device IER + IO-APIC unmask), and the
  device-IER flow-control brake; `production_external_irq_dispatch` drains
  the FIFO into the console queue on the COM1 GSI (resolved from the MADT
  interrupt-source-override for ISA IRQ 4, else identity); `serial_sink`
  installs the unlock-gated interrupt-fed read half; and `root_unlock`'s
  `X86UnlockConsole` arms the receive interrupt and hands the interactive
  read half to the unlock kthread.
- **The lossless backpressured FIFO→`ConsoleInputQueue` drain is one shared
  definition** (`kernel/tairix-kernel/src/console_uart.rs`
  `drain_fifo_into_console`, host-tested), used by both the x86_64 16550 and
  the aarch64 PL011 paths (the aarch64 `drain_uart_locked` was re-wired onto
  it, §2.2/§2.21) — only the per-UART FIFO read / receive-latch clear /
  flow-control brake are injected closures. **Regression-confirmed live**:
  `root_unlock_admission_qemu_aarch64` (which types a passphrase over the
  interrupt-driven PL011 console) still passes on a real guest boot.

**Blocked (D7).** The x86_64 live verticals
(`root_unlock_admission_qemu_x86_64`, and the `pipeline`/`uart_console`
siblings) exercise the production x86_64 kthread-admission disk bring-up,
which stalls on a live boot: the virtio-blk-PCI completion MSI-X never wakes
the scheduler-parked unlock kthread (`plans/OPEN-DEFECTS.md` D7), so the
passphrase prompt is never reached. This is the never-live-confirmed A2
production path, not the console. The `root_unlock_admission_qemu_x86_64`
bin + enrolment were authored and then removed this session to keep the gate
green; recreate them once D7 is fixed (thin bin over `tairix_kernel::boot`
with an `UnlockAdmissionSink` on `USERS_DB_INSTALLED_MESSAGE`, `EncryptedRootDisk`,
`serial: &[("ARXFS passphrase: ", …, UNLOCK_PASSPHRASE_LINE)]`).

**Also landed this increment (a live-confirmed A2/A4 discovery fix).** The
production x86_64 PCI discovery (`boot_x86_64::seed_virtio_pci`) had never
worked on a live boot: it used ECAM only, but the QEMU default `pc`/i440fx
machine exposes no MCFG/ECAM, so the root disk was never discovered. It now
prefers ECAM when the firmware advertises an MCFG (real UEFI/PCIe, `q35`)
and falls back to the universal PCI mechanism #1 (CF8/CFC port I/O)
otherwise, over one generic `probe_virtio_pci` — hardware-capability
detection, not a shim. Live-confirmed: the disk is now discovered and the
virtio-blk driver loads on the `pc` machine (the D7 stall is strictly after
that, in the completion wait).

### A4 — `devmgr` autoload over the ACPI/PCI tree (`planned`)

The x86_64 discovery emits the full generic match-key hardware tree
(block/input/display/network nodes with PCI `vendor:device:class` and
virtio ids) that the shared pre-unlock autoload path matches against the
signed driver store — the same `lib/devmatch`/`root_storage` policy code,
only the per-port node emission/probing is new (§2.21). User-space input
(PS/2 + virtio) and network drivers autoload in the production boot.
Verticals: `devmgr_hwtree_qemu_x86_64`, `autoload_input_qemu_x86_64`,
`driver_spawn_qemu_x86_64`, `driver_unload_qemu_x86_64`.

### A5 — Boot display, seat registry, graphical session (`planned`)

The VESA/GOP framebuffer feeds the shared `lib/fbcon` console engine as
the x86_64 boot display (the engine is arch-neutral; only the mode-query/
mapping glue is per-port), and the boot wires `with_seat_registry` so the
display/seat/input lease path (`plans/DISPLAY.md`) and the graphical
session work as on aarch64. Vertical: a framebuffer-console sibling of
`framebuffer_display_*` driven through the production console path, plus
the seat-lease scenario on x86_64.

### A6 — QEMU vertical parity sweep + docs (`planned`)

The remaining aarch64-only verticals gain x86_64 siblings (thin bins over
the shared scenario crates): `sandbox`, `heap`, `file_map`, `mmio_map`,
`mem_pin`, `memsoak`, `service_ceiling`/`session_ceiling`, `irq_kthread`,
`preempt_inkernel`, `stack_arena`, `sched_drive`, `ipi_smp`,
`timer_preempt` — audited against the then-current inventory when A6
starts; any vertical exercising a feature a prior increment wired lands
with that increment instead. Same-change docs: `docs/src/platform/` x86_64
page brought to the aarch64 page's level, README feature/architecture
matrix rows updated (§13).

### A7 — x86_64 hardening unblock (`blocked` on Stage 6 page-table boundary)

KPTI + IBRS/IBPB/STIBP/SSBD move from `Pending` to `Supported` in the
port's §19.1 profile the moment the Stage 6 user/kernel page-table
boundary lands ("[DO IMMEDIATELY ON UNBLOCK]", `PLAN.md`), with the
side-channel conformance vertical proving the barriers. Tracked here so
this plan is not "done" while the profile is `Pending`; gated separately
because its blocker is not an aarch64-parity item.

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
  updates this plan's status lines to the done-state summary form (§13).

## 4. Status

- **A2 `in progress`**: the production boot composition + `root_unlock`
  admission is landed and host-gate-green, and two live-boot verticals now
  pass a real guest boot — `root_unlock_login_qemu_x86_64` (the unlock
  *policy*) and `users_db_qemu_x86_64` (the boot-time users-database read
  path), both thin bins over the shared virtio-PCI scenarios (see A2 above).
  Its A1 image builder and the registry deletion remain.
  `root_unlock_admission_qemu_x86_64` moved to **A3**: the production unlock
  kthread reads `NULL_CONSOLE_READ`, so the interactive passphrase prompt it
  needs is an A3 (interrupt-driven COM1) deliverable, not an A2 thin bin.
- **A3 `in progress`**: the interrupt-driven COM1 console is implemented and
  host-tested, and its shared FIFO-drain helper is regression-confirmed live
  via the aarch64 interrupt-console vertical; the x86_64 production PCI
  disk-discovery gap it surfaced is fixed and live-confirmed. Its x86_64
  live verticals are blocked on `plans/OPEN-DEFECTS.md` D7 (the production
  MSI-X kthread disk-completion never wakes the parked bring-up), a separate
  A2 defect.
- **A1, A4, A5, A6 `planned`**; **A7 `blocked`** on the Stage 6
  user/kernel page-table boundary.
