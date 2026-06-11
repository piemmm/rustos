# Installing RustOS on a Raspberry Pi 4

`cargo xtask image --target aarch64-rpi` (equivalently `cargo xtask build
--target aarch64-rpi`, with or without `--headless`) emits
`images/rustos-aarch64-rpi.img`, a flashable SD-card image for the
Raspberry Pi 4 (BCM2711). The builder is `tools/mkimage`
(`rustos-mkimage`): pure Rust, no `parted`/`mkfs` shell-outs (`AGENTS.md`
§12), with both partitions laid down by the real in-tree filesystem
drivers.

## Image layout

| Region | Contents |
| --- | --- |
| Sector 0 | MBR: two primary partitions, `0x55AA` signature |
| Partition 1 (`0x0C`, FAT32, 64 MiB at sector 2048) | `start4.elf`, `fixup4.dat`, `bcm2711-rpi-4-b.dtb`, generated `config.txt`, `kernel8.img` |
| Partition 2 (`0x7F`, RustFS, 64 MiB) | encrypted root volume with the `AGENTS.md` §16 skeleton (`/System`, `/Users`, `/Apps`, `/Storage`) |

`kernel8.img` is the freestanding aarch64 `rustos-kernel` ELF (release
profile, linked at `0x8_0000` by `aarch64-rpi4.ld`) flattened to the raw
binary the GPU bootloader copies to memory — see the boot-protocol facts
of record in [aarch64](../platform/aarch64.md). The generated
`config.txt` sets `arm_64bit=1`, `kernel=kernel8.img`, and
`enable_uart=1` (plus `armstub=armstub8.bin` only when that optional stub
is staged).

## The firmware blobs

The Pi boot firmware is third-party redistributable binary (Broadcom /
Raspberry Pi Ltd) and is **not** committed to this repository. It is a
pinned, checksummed build input (`AGENTS.md` §19.3): the manifest
`tools/mkimage/firmware.lock` pins the upstream HTTPS `source` directory
and the exact byte length and SHA-256 of each required file at the
pinned release, and the build refuses — fail closed — any missing,
resized, or altered blob.

No staging step is needed: `cargo xtask image` fetches any blob missing
from its cache (`target/pi-firmware/`) from the pinned source via
`curl` (HTTPS only) and verifies every byte against the manifest before
use — a download that fails the checksum gate is deleted and the build
stops. Verified blobs are fetched once and reused across builds.

For air-gapped or pre-staged builds, place the manifest's files in a
directory yourself and pass `--firmware <dir>` or set
`RUSTOS_PI_FIRMWARE=<dir>`; an operator-staged directory is only ever
verified, never written. (The standalone `rustos-mkimage rpi` CLI always
takes `--firmware`: `tools/mkimage` itself performs no network I/O,
`AGENTS.md` §12.)

The optional `armstub8.bin` (the PSCI secondary-core stub) has no
official binary release; first boot does not need it because the kernel's
boot stub parks the secondary cores itself. When SMP-on-metal pins a
stub build, its hash joins `firmware.lock` and the generated `config.txt`
gains the `armstub=` knob automatically.

## Building the image

```sh
cargo xtask image --target aarch64-rpi
```

This one step builds the aarch64 kernel, fetches any missing firmware
blob from the pinned source, verifies the firmware against the
manifest, and writes:

- `images/rustos-aarch64-rpi.img` — the flashable image.
- `images/rustos-aarch64-rpi.rootkey` — the root volume key (64 hex
  digits, owner-readable only).

RustFS has no plaintext mode, so the root partition is provisioned under
a fresh random volume key on every build. **Keep the `.rootkey` file**:
mounting the root volume requires it, and it exists nowhere inside the
image. Pass `--root-key <file>` to rebuild an image under a previously
generated key. The first-boot installer (`AGENTS.md` §11, PLAN.md
Stage 8) re-provisions the volume under the user's own credentials.

## Flashing and first boot

Write the image to an SD card (replace `sdX` with the card device — this
destroys its contents):

```sh
sudo dd if=images/rustos-aarch64-rpi.img of=/dev/sdX bs=4M conv=fsync
```

Connect a 3.3 V serial adapter to the Pi's UART header (GPIO 14/15,
115200 8N1), insert the card, and power on. The firmware loads
`kernel8.img` at `0x8_0000` and enters it at EL2 with the DTB pointer in
`x0`; the kernel discovers the PL011 console, GIC, timer, and memory map
from that device tree — the same code path the QEMU `virt` board boots —
and brings up user mode (PID 1 `init` spawning the shell over the
standard streams).

The on-metal bring-up checklist and its UART-log acceptance artefacts
are tracked per stage in `plans/PI.md` (P7–P10); QEMU has no usable Pi-4
model, so real hardware is the acceptance environment for the
peripherals (`plans/PI.md` §0.4).
