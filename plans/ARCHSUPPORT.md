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
  `rustos_drv_storage_virtio_blk` floor entry over the virtio-PCI
  provisioning seam (`kernel/rustos-kernel/src/x86_64/virtio_boot.rs`),
  bound because the ACPI/PCI-enumerated hardware tree matched its bind
  table through the shared `lib/devmatch` policy — no compile-time device
  address, no `cfg` fork outside the port (§2.20).
- **Each increment lands complete and green.** No "for now" shims (§2.19):
  an increment that wires the unlock pipeline wires the whole pipeline
  (unlock kthread → RustFS root mount → users DB publish → `/System` mount
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
  `fat32_virtio_blk_pci_x86_64`, `rustfs_virtio_blk_pci_x86_64`,
  `vesa_display_qemu_x86_64`, `ps2_input_qemu_x86_64`, `irq_qemu_x86_64`;
  the virtio-PCI provisioning seam and an in-kernel driver host
  (`x86_64/driver_host.rs`) exist.
- **Production boot gap (the whole point of this plan):**
  `kernel/rustos-kernel/src/x86_64/boot.rs` wires only COM1 consoles,
  installed memory, `LATE_IDENTITY`, init spawn, the embedded-registry
  spawn producer, and the ACPI hardware-tree source. It has no
  `root_unlock` module, no `with_app_store`/`with_users_db`/
  `with_filesystem`/`with_volumes`/`with_seat_registry`, and no `devmgr`
  autoload admission — all of which the aarch64 boot
  (`src/aarch64/{boot,root_unlock}.rs`) already composes from shared code.

## 2. Increments (dependency order; each fully gated per §7)

### A1 — `tools/mkimage` x86_64 image builder (`planned`)

The Stage 8 deliverable `images/rustos-x86_64.iso` / bootable disk image
(§12): GPT layout (hybrid BIOS/UEFI boot is the §12 target; the increment
lands whatever QEMU boots the kernel from today, complete for that path —
if genuine BIOS+UEFI hybrid boot needs boot-loader work beyond this plan,
that is surfaced under §15.7 before A1 is scoped, never stubbed), a FAT
boot partition carrying the kernel and the `root.unlock` descriptor, and
an encrypted RustFS root with the §16 skeleton — reusing the existing
pure-Rust rootfs/partition/appload planting code (`build_system_partition`,
the `image_apps`/`image_drivers` pipelines) unchanged. Deliverables: the
`--target x86_64` builder in `tools/mkimage` with `installer`/`debug`
profiles matching the Pi builder's semantics, host tests over the produced
layout, and the QEMU whole-disk fixture able to serve the same image shape
the verticals mount.

### A2 — Production boot storage floor + registry deletion (`planned`)

The `PLAN.md` increment-5 end state. Bind the virtio-blk-PCI root through
the shared `root_storage` gate in the x86_64 production boot; add the
port's `root_unlock` admission (the unlock kthread composing the shared
`root_mount` pipeline: PBKDF2 unlock → RustFS root mount → users DB +
admin publish → read-only `/System` mount → volume forest → disk-backed
app store), wiring `with_app_store`/`with_users_db`/`with_users_admin`/
`with_filesystem`/`with_volumes` exactly as the aarch64 boot does. riscv64
gains the same over its virtio-MMIO floor (same increment or the next).
Then **delete** `SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s (all but PID 1
`init`), `spawn_paths.rs`, and `program_manifests.rs`, updating `PLAN.md`
(§2.14). Verticals: `root_unlock_login_qemu_x86_64`,
`root_unlock_admission_qemu_x86_64`, `users_db_qemu_x86_64` as thin bins
over the shared scenarios.

### A3 — Interrupt-driven console + login/session supervision (`planned`)

COM1 console input moves from the polled cooperative shim to
interrupt-driven wake-ups through `kernel/irq` (the `PLAN.md`
"interrupt-driven ps2/virtio wake-ups" note), so a parked reader wakes on
the IRQ, never a poll loop (§2.23). The production boot then supervises the
same arch-neutral `login` → session pipeline off the disk store (the
binaries are the same bundles A2 mounts). Verticals:
`uart_console_qemu_x86_64` (COM1 sibling of the aarch64 UART scenario),
`pipeline_qemu_x86_64`.

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
  code lives under `kernel/rustos-kernel/src/x86_64/` or
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

All increments `planned` (A7 `blocked` on Stage 6) — nothing started.
