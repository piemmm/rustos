# Installing RustOS on a Raspberry Pi 4

`cargo xtask image --target aarch64-rpi` (equivalently `cargo xtask build
--target aarch64-rpi`, with or without `--headless`) emits
`images/rustos-aarch64-rpi-<profile>.img`, a flashable SD-card image for
the Raspberry Pi 4 (BCM2711). The builder is `tools/mkimage`
(`rustos-mkimage`): pure Rust, no `parted`/`mkfs` shell-outs (`AGENTS.md`
§12), with every partition laid down by the real in-tree filesystem
drivers.

Both profiles are also assembled end-to-end by the `cargo xtask ci`
image gate on every change, so an image-breaking change cannot land
green.

Two image profiles exist (`--profile`, default `debug`):

- **`debug`** — the development image: the root volume is seeded with a
  `/System/Security/Users` database carrying the single test account
  `root` / `root` (salted and hashed per build, `lib/users`), so the
  login prompt is usable without running the installer. A debug image
  must never ship.
- **`installer`** — the shippable image: no user accounts; the first-boot
  installer (`AGENTS.md` §11) authors the user database.

## Image layout

| Region | Contents |
| --- | --- |
| Sector 0 | MBR: three primary partitions, `0x55AA` signature |
| Partition 1 (`0x0C`, FAT32, 64 MiB at sector 2048) | `start4.elf`, `fixup4.dat`, `bcm2711-rpi-4-b.dtb`, `overlays/disable-bt.dtbo`, generated `config.txt`, `kernel8.img`, `root.unlock` |
| Partition 2 (`0x7E`, RustFS, 64 MiB) | read-only, signed-bundle `/System` volume (the §16.2 skeleton); keyed by the non-secret well-known `SYSTEM_VOLUME_KEY` (effectively unencrypted — integrity rests on the per-bundle signatures, `AGENTS.md` §18.6), mounted read-only before unlock (the design-B pre-unlock driver store, `plans/PI.md`) |
| Partition 3 (`0x7F`, RustFS, 64 MiB) | encrypted data-root volume with the `AGENTS.md` §16 skeleton (`/Users`, `/Apps`, `/Storage`, `/System/Security`), unlocked by a passphrase-derived key |

`root.unlock` is the root volume's plaintext key-derivation descriptor
(`AGENTS.md` §11): the per-volume random salt and PBKDF2 iteration count
the bootstrap reads — before anything is decrypted — to turn the
operator passphrase into the volume key. It is the analogue of a LUKS
header and carries no secret. The passphrase is never stored on the
image.

`kernel8.img` is the freestanding aarch64 `rustos-kernel` ELF (release
profile, linked at `0x8_0000` by `aarch64-rpi4.ld`) flattened to the raw
binary the GPU bootloader copies to memory — see the boot-protocol facts
of record in [aarch64](../platform/aarch64.md). The generated
`config.txt` sets `arm_64bit=1`, `kernel=kernel8.img`, `enable_uart=1`,
`dtoverlay=disable-bt` (the PL011 `UART0` on the GPIO 14/15 header — the
overlay itself is a pinned firmware input staged on the partition at
`overlays/disable-bt.dtbo`), `init_uart_clock=48000000` (the PL011
reference clock the kernel's baud divisors assume), and
`init_uart_baud=9600` — the kernel programs the serial console itself to
**9600 8N1** at boot (`rustos_arch_aarch64::uart_init`: GPIO 14/15 pin
mux plus PL011 line registers; plus `armstub=armstub8.bin` only when
that optional stub is staged).

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
manifest, and writes (for the default `debug` profile; pass
`--profile installer` for the installer image):

- `images/rustos-aarch64-rpi-debug.img` — the flashable image.
- `images/rustos-aarch64-rpi-debug.rootkey` — the derived root volume key
  (64 hex digits, owner-readable only).

RustFS has no plaintext mode, so the root partition is always encrypted.
Its volume key is **derived from a passphrase** (`AGENTS.md` §11): the
build provisions a per-volume `root.unlock` descriptor (random salt +
PBKDF2 cost), runs the passphrase through it to a 256-bit key, and
provisions the root under that key. Both `mkimage` profiles use a
**blank** passphrase — these are special-case images (the debug image
must never ship; the installer image's root is re-provisioned on first
boot), so neither prompts and the key is auto-derived. The volume is
still fully encrypted under a real, salt-derived key. The `.rootkey`
file is that derived key, written for mounting the volume on a host; it
can equally be re-derived from the on-image `root.unlock` descriptor and
the blank passphrase.

A shippable, user-installed root is different: the first-boot installer
(`AGENTS.md` §11, PLAN.md Stage 8) provisions the volume under a
passphrase the **user chooses**, writes its `root.unlock` descriptor,
and the production boot then prompts for that passphrase before mounting.
A blank default is never used for a user's own data.

## Flashing and first boot

Write the image to an SD card (replace `sdX` with the card device — this
destroys its contents):

```sh
sudo dd if=images/rustos-aarch64-rpi-debug.img of=/dev/sdX bs=4M conv=fsync
```

Connect a 3.3 V serial adapter to the Pi's UART header (GPIO 14/15,
9600 8N1), insert the card, and power on. The firmware loads
`kernel8.img` at `0x8_0000` and enters it at EL2 with the DTB pointer in
`x0`; the kernel discovers the PL011 console, GIC, timer, and memory map
from that device tree — the same code path the QEMU `virt` board boots —
and brings up user mode (PID 1 `init` spawning the shell over the
standard streams).

The on-metal bring-up checklist and its UART-log acceptance artefacts
are tracked per stage in `plans/PI.md` (P7–P10); QEMU has no usable Pi-4
model, so real hardware is the acceptance environment for the
peripherals (`plans/PI.md` §0.4).
