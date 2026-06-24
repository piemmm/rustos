# rustos-vcmailbox

The single **BCM2711 `VideoCore` firmware mailbox property-channel client**
(`AGENTS.md` §2.2 — `plans/PI.md` P7/P7b).

On a Raspberry Pi the GPU firmware owns the display pipeline; the ARM side
talks to it over the mailbox property channel — a 16-byte-aligned buffer of
little-endian `u32` tag words posted through the doorbell registers. This
crate owns that protocol once:

- the **pure framing layer**: `FramebufferRequest::encode` /
  `decode_framebuffer_response` (allocate a scan-out surface) and
  `encode_display_size_query` / `decode_display_size_response` (probe the
  attached display's EDID-derived geometry; `0×0` means no display). Every
  firmware answer is validated fail-closed — the firmware is an external
  input (`AGENTS.md` §5.4).
- the **bus ↔ ARM-physical translation** (`bus_to_arm_physical`,
  `arm_physical_to_bus`, `DEFAULT_BUS_ALIAS`) over the 30-bit `VideoCore`
  SDRAM aperture, failing closed on anything outside it.
- the **transport seam**: `MailboxTransport` with `MmioMailbox` as the metal
  doorbell implementation over two capability-gated `RegisterWindow`s. QEMU
  does not model the firmware, so host tests drive the seam with a
  protocol-faithful mock and the doorbell is the on-metal acceptance item
  (`AGENTS.md` §2.1).

## Why it lives in `lib/` (the §2.20 / §2.22 carve-out, legitimately)

This is single-device support — it knows the BCM2711 `VideoCore` — yet it
**stays** in `lib/*`, unlike the VL805 / BCM2711-PCIe device logic, which was
collapsed into its driver crate (`AGENTS.md` §2.22). The difference is the
second consumer: two independent consumers speak this protocol — the aarch64
port's framebuffer boot console (`kernel/arch/aarch64`, P7b) and the HVS
display driver (`drivers/display/rpi_hvs`, P7). The boot console is a
**charter-legal non-driver** consumer (a genuine early-boot need, not a
removable scaffold), so the §2.20 carve-out applies and the shared definition
belongs in `lib/*` (`AGENTS.md` §2.22 / §6 / §2.2); a driver crate may not be
a kernel dependency (`AGENTS.md` §17.4). By contrast the VL805 / PCIe device
logic had only a `drivers/*` consumer (its illegitimate second consumer was a
removed in-kernel scaffold), so it has no `lib/*` home. This crate depends
only on `lib/abi` (the `DisplayFormat` / `RegisterWindow` / `DriverError`
vocabulary).

## Stability tier

`experimental` — the Raspberry Pi bring-up firmware seam. It is `no_std`,
contains no `unsafe` (`#![forbid(unsafe_op_in_unsafe_fn)]`, no `unsafe`
blocks), and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9).
