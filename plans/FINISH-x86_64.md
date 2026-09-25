# FINISH-x86_64 — an x86_64 image that boots to the desktop

Binding under `AGENTS.md`. The staged plan for taking the x86_64 port from
"production kernel, text login and shell in QEMU" to "a disk image that boots
from UEFI firmware to the graphical desktop in QEMU", on exactly the boot and
display path a real UEFI PC uses. A physical target — the Surface Go 2 — is the
next stage (§5) and is out of scope until this plan is done.

Read first: `plans/ARCHSUPPORT.md` (x86_64 product parity; this plan delivers
its A5 and depends on its A2/A4), `plans/BOOTLOADER.md` (the Rust UEFI loader
and GPT image this plan builds on — B3/B4 are owned there, not here),
`plans/DISPLAY.md` (seats, the display service, the boot-display publication),
`plans/NEW-DESKTOP-LOGIN.md` (the text-vs-graphical decision and the greeter),
and `plans/FIX-DISPLAY-ACCELERATION.md` (the display ABI the scan-out feeds).

## Ledger

| Id | Item | Status |
|---|---|---|
| F1 | x86_64 write-combining memory type (IA32_PAT) | planned |
| F2 | One shared boot framebuffer console and console/seat layout for every port | planned |
| F3 | The UEFI loader hands the kernel a GOP framebuffer (`plans/BOOTLOADER.md` B3) | blocked: the §4 measurement decision |
| F4 | GPT/ESP whole-disk image, `cargo xtask image --target x86_64`, `run --target x86_64` (`plans/BOOTLOADER.md` B4) | blocked: F3 |
| F5 | x86_64 boot display: GOP scan-out → boot console, display node, seat registry | blocked: F1, F2, F3 |
| F6 | Display service and pointer input autoload on x86_64 | blocked: F4, F5 |
| F7 | Graphical session verticals: greeter by default, then the revealed desktop | blocked: F6 |
| F8 | Platform page, README matrix, and `plans/ARCHSUPPORT.md` A5 closed | blocked: F7 |

## 1. What exists, and what is missing

The production x86_64 boot (`kernel/tairix-kernel/src/x86_64/boot.rs`)
already reaches a text session in QEMU from a planted whole-disk image: ACPI
and PCI discovery, the encrypted-root unlock over virtio-blk-PCI with MSI-X,
the read-only `/System` store, the users database, the interrupt-driven COM1
console, `login`, the shell, and `devmgr` autoloading signed drivers into
their own processes (keyboard, network, sound). `tools/qemu` already renders
windowed interactive x86_64 sessions with virtio keyboard and mouse over PCI.

Missing, in the order the chain needs it:

- **No framebuffer reaches the kernel.** QEMU's `-kernel` PVH boot hands over
  none, and the multiboot2 framebuffer tag the kernel can parse needs a boot
  loader to produce it.
- **Write-combining is refused.** `kernel/arch/x86_64/src/paging.rs` returns
  `MapError::Unsupported` for `PageFlags::WRITE_COMBINE`, because the port
  never programs the PAT. A display aperture is `FramebufferMemory::WriteCombine`,
  so the display service could not map the screen even if one were published.
- **No console or seat layout.** The boot calls neither
  `boot_display::observe_boot_display` nor `with_seat_registry`; it lists COM1
  alone.
- **The boot framebuffer console lives in one port.** Its renderer, surface
  publication, handover and purge sit in `kernel/arch/aarch64/src/video.rs`,
  and the console/seat layout in `kernel/tairix-kernel/src/aarch64/arch_wrapper.rs`.
  Only surface discovery and cache maintenance there are aarch64's own.
- **No image and no `run`.** `cargo xtask image` and `run` accept only
  `aarch64-rpi`.

## 2. Binding decisions

- **One display path, the real one.** The desktop's screen in QEMU is the UEFI
  **GOP** framebuffer, set up by OVMF, handed over by the first-party loader in
  the multiboot2 framebuffer tag — the same chain a physical UEFI machine runs,
  and the model Linux's `efifb`/`simpledrm` uses. Rejected alternatives:
  ramfb over fw_cfg under `-kernel` (on x86 fw_cfg is found only through the
  ACPI namespace or a blind fixed-port probe, and the path would not exist on
  real hardware), and a bochs-display PCI driver (QEMU/VirtualBox only).
- **PVH stays the fast test path, and a boot without a framebuffer is
  first-class.** Verticals that need no display keep booting with `-kernel`.
  A boot with no framebuffer tag selects the COM1-only console layout, and
  login proceeds in text; that is the headless configuration, not a fallback.
- **OVMF is the pinned QEMU build's own `edk2-x86_64-code.fd`**, installed by
  `tools/ci/install-qemu.sh` from GPG-verified QEMU source. It is not fetched,
  vendored, or pinned separately. Each run takes a fresh writable copy of the
  variable store, so no state leaks between runs.
- **Firmware in the loop must be deterministic; if it isn't, that is a
  defect.** The harness booted through OVMF and GRUB until it moved to PVH
  (`tools/qemu/src/x86_64.rs`). The recorded reason was that OVMF's video path
  crashed nondeterministically, and no root cause was ever given. The OVMF
  verticals here are run repeatedly under the loaded matrix, and any
  nondeterminism is diagnosed to its cause — ours, OVMF's, or QEMU's — before
  the item is done. Firmware boot is a separate `tools/qemu` boot kind beside
  PVH. The existing test that PVH argv carries no firmware stays, because it
  still describes the PVH kind exactly.
- **The kernel trusts the framebuffer tag no more than any other boot input.**
  The scan-out is admitted only as 32-bpp direct RGB, with base, pitch and
  extent free of overflow, the pitch covering a row, and the aperture outside
  every usable RAM range of the memory map. Anything else is refused and the
  boot proceeds headless.
- **The display is reached only through discovery.** The kernel publishes the
  admitted scan-out as the boot-display hardware-tree node
  (`boot_display::observe_boot_display`). The display service autoloads
  against it through `devmgr` and receives one write-combining framebuffer
  grant, exactly as on aarch64. Nothing below the display service names a
  display device.
- **Shared first, then wired.** The console renderer and the console/seat
  layout are hoisted into shared homes (F2) before x86_64 uses them. A second
  copy for x86_64 is the duplication the charter forbids.

## 3. Increments

### F1 — x86_64 write-combining

The port programs `IA32_PAT` (MSR `0x277`) at CPU setup, identically on the
BSP and on every AP before that CPU maps anything. The Intel SDM requires the
PAT to agree across processors. Entries 0–3 keep their power-on values
(WB, WT, UC-, UC), so the existing PWT/PCD encodings keep their meaning.
Entry 4 becomes WC. `PageFlags::WRITE_COMBINE` then encodes as PAT index 4:
bit 7 on a 4 KiB leaf, bit 12 on a 2 MiB or 1 GiB leaf. Every x86_64
leaf-encoding path (kernel and user address spaces, the MMIO mapper) accepts
it, and the flag decode reads it back. CPUID.01H:EDX.PAT absent refuses
write-combining (`Unsupported`) rather than silently mapping UC.

The aperture must not be aliased with a conflicting memory type, which the
SDM leaves undefined. The kernel therefore never maps the aperture itself
other than write-combining. F1 audits the boot identity map's treatment of
non-RAM ranges and fixes it if the aperture could be reached through a
cacheable alias.

Tests: host tests of the PAT value and of the leaf encode/decode round trip at
every page size; the x86_64 conformance vertical maps a write-combining page;
`vesa_display_qemu_x86_64` changes from a default `map_window` to a
write-combining mapping of its surface.

riscv64 refuses `WRITE_COMBINE` the same way (Svpbmt `NC` is the encoding). It
has no display path yet, so the refusal is honest fail-closed behaviour; its
encoding lands with riscv64's boot display, not here.

### F2 — one boot framebuffer console, one console/seat layout

The arch-neutral half of `kernel/arch/aarch64/src/video.rs` moves to a shared
home (`lib/fbcon`, which already owns the `TextConsole` engine): the surface
state, the render lock, attach, `write_bytes`, the surface disposition
(`set_surface` / `reclaim_surface` / `purge`), and `paint`. The port supplies
only what differs: where the surface comes from, the post-write cache
maintenance (aarch64 cleans to the point of coherency; x86_64 writes through a
write-combining mapping and needs none), and the interrupt-control type the
render lock masks with.

The hoist fixes `plans/OPEN-DEFECTS.md` D152 in the shared code, so both ports
get the fix: a report raised while the render lock is held must not block on
that lock. D152 closes with its regression test in this increment.

`console_layout` and its `VIDEO_*` / `UART_*` console-list and seat-registry
statics move from `aarch64/arch_wrapper.rs` into one shared
`kernel/tairix-kernel` module, parameterised on the port's framebuffer and
serial console devices. It keeps the invariant that the seat's text sink is
the console listed at index 0.

aarch64 is rewired onto both in the same change. Its framebuffer, greeter and
desktop verticals are the regression evidence. No aarch64 behaviour changes.

### F3 — the loader hands over a GOP framebuffer

Delivered by `plans/BOOTLOADER.md` B3. What this plan needs from it is
recorded there: the GOP mode choice, the framebuffer tag, and the firmware
entropy seed. The kernel's multiboot2 entry has not run on a live boot since
the harness moved to PVH; its loader-agnostic boot-data parser
(`kernel/arch/x86_64/src/bootinfo.rs`) is host-tested only. B3's OVMF vertical
is its first live boot on the current tree.

### F4 — the whole-disk image, `image` and `run`

Delivered by `plans/BOOTLOADER.md` B4 (GPT encode in `lib/partition`, the ESP
carrying the loader, the kernel and `root.unlock`, then the `/System` store
and the encrypted root). The ESP holds no other file; everything else is on
the `/System` volume.

The composition `build_rpi_image` performs above its boot partition (users
and groups databases, home, attestation key, machine id, library catalogue,
the `/System` and root partitions) is hoisted into one function both image
builders call. Only the boot partition and the partition-table scheme differ.

This plan owns the xtask surface:

- `cargo xtask image --target x86_64 [--profile debug|installer]` writes
  `images/tairix-x86_64-<profile>.img`, a raw GPT disk that UEFI firmware
  boots from a USB stick as written. The hybrid BIOS/UEFI
  `images/tairix-x86_64.iso` the charter names is the same layout with
  `plans/BOOTLOADER.md` B5's BIOS half added, and is not needed for QEMU or a
  UEFI-only machine.
- `cargo xtask run --target x86_64` boots that image windowed: OVMF, the disk
  as virtio-blk-PCI, the default VGA device for GOP, virtio keyboard and mouse,
  and virtio-net on user networking pinned to the MAC the shipped
  `network.conf` binds. No `-kernel` appears in the command line.

The run spec is a pure, host-tested function, as `run_session_spec` is for
aarch64.

### F5 — the x86_64 boot display

The x86_64 boot reads the multiboot2 framebuffer tag and admits it into a
`BootScanout` under the §2 checks, with `FramebufferMemory::WriteCombine`. It
maps the aperture write-combining and attaches the shared boot console (F2).
Kernel output then reaches the screen as well as COM1. The boot publishes the
boot-display node, and chooses its console list and seat registry through the
shared layout: framebuffer plus COM1 when a scan-out was admitted, COM1 alone
otherwise.

The kernel console yields the surface to the display service through the same
reclaim and purge disposition aarch64 uses, so no two writers share a scan-out.

Vertical: an OVMF boot whose serial transcript witnesses the boot-display node
and whose host-side screendump shows console text.

### F6 — display service and pointer on x86_64

The x86_64 image's driver set gains the framebuffer display service and the
virtio-input driver, both built for x86_64 by the existing `image_drivers`
pipeline. `devmgr` autoloads the display service against the boot-display
node and the input driver against each virtio-input PCI function, keyboard and
pointer alike. The input driver is transport-neutral and already binds over
PCI.

Vertical: the display service's first present, plus a pointer event delivered
to the input-focus arbiter over virtio-PCI.

### F7 — the graphical session

Two x86_64 siblings of existing aarch64 verticals, each a thin bin over the
shared scenario code with no second scenario implementation:

- `greeter_default_qemu_x86_64`: an unconfigured machine that can draw boots
  to the greeter.
- A desktop-reveal vertical: log in, reach `desktop fully revealed`, and
  deliver a pointer event to the desktop.

Both boot through OVMF from the F4 image. The x86_64 verticals that need no
display keep PVH, so firmware start-up cost is paid only where it is under
test.

### F8 — docs and closure

- `docs/src/platform/x86_64.md` gains the boot-display, PAT and GOP sections.
- A new `docs/src/install/x86_64.md`, listed in `docs/src/SUMMARY.md`,
  describes `image` and `run`.
- The README rows for framebuffer/display, graphical login and bootable image
  move to their real x86_64 state.
- `plans/ARCHSUPPORT.md` A5 is marked done.

## 4. Open decisions

- **Loader measurement without a TPM.** `plans/BOOTLOADER.md` requires that the
  loader never launch an image it could not *verify and measure*. OVMF in QEMU
  has no TPM unless an external `swtpm` is attached, so as written F3 cannot
  boot. Either the harness attaches a software TPM, or the policy becomes
  "always verify; measure when a TPM is present, and record its absence". This
  is a security-policy choice for the user, made before B3 lands.

## 5. Next stage: Surface Go 2 (not started)

Out of scope until F8 is done. What the tree already shows the physical
machine needs beyond this plan; the exact parts are confirmed by S0.

| Id | Item | Status |
|---|---|---|
| S0 | Hardware survey of the target: CPU, storage controller, display, input, radios, and the ACPI namespace | planned |
| S1 | Secure Boot: sign the loader for the machine's key database, or document the disable step (a decision for the user) | planned |
| S2 | xHCI bound by its PCI class over the PCI tree (today `drivers/bus/usb/xhci` binds only the node the Pi's VL805 bridge emits) | planned |
| S3 | A driver for the machine's internal storage (no NVMe, AHCI or SDHCI driver exists in `drivers/storage`) | planned |
| S4 | An ACPI namespace (AML) reader: power-off (`plans/ARCHSUPPORT.md` A7), the power button, battery, the i8042 (`PNP0303`), and ACPI-enumerated I2C/HID devices | planned |
| S5 | Real-silicon hardening and robustness: `plans/ARCHSUPPORT.md` A8 (KPTI, speculation barriers), `plans/OPEN-DEFECTS.md` D85 (spurious LAPIC interrupt), D3 (hard-lockup watchdog), D210 (`refuse()` exits through the QEMU debug port) | planned |
