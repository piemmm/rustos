# tairix-fwcfg

**Stability tier: stable.**

The single **QEMU `fw_cfg` DMA client and `ramfb` programming helper**
(`AGENTS.md` §2.2).

QEMU's `fw_cfg` device exposes firmware configuration items (and the
`ramfb` display's `etc/ramfb` control file) through one DMA protocol: an
in-RAM big-endian `FWCfgDmaAccess` staging structure whose physical
address is written to the device's 64-bit big-endian DMA address
register. This crate owns that protocol once:

- the **transport seam**: `DmaAddressRegister` — the one genuinely
  platform-specific fact (the register is MMIO at `base + 16` on the
  Arm/riscv `virt` boards, I/O ports `0x514`/`0x518` on x86). The MMIO
  transport (`MmioDma`, discovered from the device tree's
  `qemu,fw-cfg-mmio` node) lives here because the two `virt` boards
  expose it identically; the x86 I/O-port transport stays in its own
  vertical.
- the **DMA client** (`FwCfg`): select/read/write items, the `QEMU`
  signature round-trip, and the bounded, allocation-free file-directory
  scan (`file_selector`) — every buffer is a stack or caller-supplied
  slice, so the pre-heap aarch64 boot console can drive it.
- the **`ramfb` programming helper** (`RamfbConfig`, `program_ramfb`):
  the 28-byte big-endian `RAMFBCfg` wire form pointing the device's
  scan-out at a guest-RAM surface.

Consumers: the aarch64 framebuffer boot console's QEMU `virt` ramfb path
(`kernel/arch/aarch64::video`) and the display-class QEMU verticals
(`tests/integration/framebuffer_display_qemu_{aarch64,riscv64}`,
`tests/integration/vesa_display_qemu_x86_64`).

Like `lib/vcmailbox` (the Pi analogue), this is device support with a
legitimate non-driver consumer — the boot console, a genuine early-boot
need — so it lives in `lib/*` under the single-device carve-out.

All firmware answers are treated as untrusted input and validated
fail-closed: the directory scan is capped (`MAX_DIR_ENTRIES`), transfers
confirm the device cleared `control`, and every failure is a typed
`FwCfgError`.
